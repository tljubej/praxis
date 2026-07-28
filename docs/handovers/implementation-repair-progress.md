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
| S7 — Descriptor totality, typed collections, fault representation | **done** | `d014067`, `c00aab7`, `6da6037`, `eda8c69` |
| S8 — Generation arena for JIT and plan metadata | **done** | `1311132`, `b60da0a` |
| S9 — MIR root exactness, debug/root split, verifier | **done** | `ad9bbdf`, `d9521f9`, `e15a444` |
| S10 — Semantic comparison, nominal schema identity | **RT-16 only** | `29ff4f6` |
| S11 … S21 | not started | |

Also closed out of order: **DBG-01** (`3836b74`), a P0 the plan schedules in
S10. It fell out of S1. **DBG-02** is closed in part (see §6).

Baseline at `136ce4b` was **928 passed, 0 failed, 149 ignored**.
Now: **1096 passed, 0 failed, 106 ignored**. `just ci` is green.

Forty-two of the audit's ignored regressions are un-ignored and passing.
The three added by S9:

| Test | File | Finding |
|---|---|---|
| `local_dead_after_its_last_use_is_not_rooted_at_a_later_safepoint` | `liveness.rs` | MIR-02 |
| `exact_roots_shrink_between_two_safepoints_in_one_block` | `liveness.rs` | MIR-02 |
| `empty_element_returning_sinks_fault_instead_of_returning_uninitialized_gc_refs` | `adversarial_audit.rs` | MIR-09 |

S9's fifteen new gates:

| Test | File | Pins |
|---|---|---|
| `a_slot_spilled_at_one_safepoint_is_nulled_once_its_local_dies` | `liveness.rs` | MIR-01 — the `dead` set exists and is computed |
| `the_live_and_dead_sets_of_a_safepoint_are_disjoint` | `liveness.rs` | a slot is spilled or nulled, never both |
| `the_debug_set_still_shows_what_the_root_set_dropped` | `liveness.rs` | MIR-16 — H3's property, stated where it lives |
| `check_fault_carries_an_annotated_debug_set` | `liveness.rs` | the debugger safepoint that is not a GC one |
| `a_dead_local_stops_being_reachable_from_its_frame` | `adversarial_audit.rs` | MIR-01 end to end — 3000 elements that used to survive |
| `an_unannotated_set_is_empty_and_says_so` | `annot.rs` | the seal |
| `an_annotated_empty_set_is_not_an_unannotated_one` | `annot.rs` | "nothing is live" ≠ "the pass never ran" |
| `a_scalar_local_in_the_root_set_is_rejected` | `verify.rs` | the plan's first named verifier gate |
| `an_out_of_range_jump_target_is_rejected` | `verify.rs` | the plan's second (MIR-11's class) |
| `a_move_gc_out_of_a_scalar_is_rejected` | `verify.rs` | P0-03, now enforced rather than documented |
| `an_unannotated_safepoint_is_rejected` | `verify.rs` | what the seal buys the verifier |
| `returning_a_scalar_is_rejected` | `verify.rs` | the ABI returns a `GcRef` |
| `a_bounded_division_is_rejected` | `verify.rs` | `sdiv` traps; no bound rules out a zero divisor |
| `an_out_of_range_operand_is_rejected` | `verify.rs` | the cheap builder-bug shape |
| `an_annotated_function_verifies` | `verify.rs` | the rules are satisfiable |

`adv_pipeline_empty_source_reduce` (`jit.rs`) was **rewritten**, not
un-ignored: it asserted only that the host survived, because there was no
contract to assert. There is one now.

RT-16's five gates (S10's only landed finding; it had none, per plan §8.3):

| Test | File | Pins |
|---|---|---|
| `map_formatting_does_not_follow_hash_table_order` | `maps.rs` | RT-16 — the same map renders one way |
| `set_formatting_does_not_follow_hash_table_order` | `maps.rs` | RT-16 |
| `counter_formatting_does_not_follow_hash_table_order` | `maps.rs` | RT-16 |
| `heap_formatting_does_not_depend_on_insertion_order` | `heaps.rs` | RT-16 — and asserts the two backing arrays really differ |
| `a_min_heap_renders_smallest_first` | `heaps.rs` | pop order through `Reverse` |

The one added by S8:

| Test | File | Finding |
|---|---|---|
| `record_schema_cache_is_scoped_by_type_database_not_bare_def_id` | `adversarial_audit.rs` | MIR-12, DBG-06 |

S8's fifteen new gates, for the four findings that had none:

| Test | File | Pins |
|---|---|---|
| `the_same_def_id_in_two_generations_gets_two_schemas` | `generation.rs` | MIR-12/DBG-06 at the unit level |
| `one_def_id_in_one_generation_is_one_schema` | `generation.rs` | the sharing the fix must *not* break |
| `a_failed_schema_build_caches_nothing` | `generation.rs` | a D9 refusal leaves no half-schema |
| `tuple_schemas_are_shared_by_shape` | `generation.rs` | structural keying survives the move |
| `repeated_identical_metadata_stops_growing_the_arena` | `generation.rs` | DBG-05/MIR-13 — interning, not just reclaiming |
| `a_retired_generation_releases_its_arena` | `generation.rs` | H15 — `retire` needs the proof to compile |
| `a_shared_generation_survives_a_partial_retire` | `generation.rs` | a second handle keeps its pointers |
| `generation_ids_are_distinct_and_nonzero` | `generation.rs` | the key half that is not the def id |
| `repeated_evaluation_stops_growing_the_generation` | `evaluate.rs` | DBG-05 through the *real* `p` path |
| `zero_is_not_a_plan_id` | `plan.rs` | IP-12 — the sentinel has no encoding |
| `registered_plans_round_trip_through_their_raw_id` | `plan.rs` | IP-12 — the MIR immediate round-trips |
| `an_unregistered_id_resolves_to_nothing` | `plan.rs` | IP-12 — out of range is `None`, not an index |
| `a_compiled_plan_owns_its_interned_strings` | `plan.rs` | IP-12 — the arena really owns them |
| `registration_past_the_bound_is_refused` | `plan.rs` | IP-12 — bounded, and refuses before pushing |
| `retiring_parser_plans_empties_the_arena` | `teardown.rs` | IP-12 — plans and schemas go together |

The thirteen added by S7's second half:

| Test | File | Finding |
|---|---|---|
| `dynamic_keys_with_different_descriptors_are_never_equal` | `praxis-runtime/src/dynamic_key.rs` | RT-09 |
| `vec_push_rejects_a_value_with_the_wrong_descriptor` | `praxis-runtime/src/abi.rs` | P0-11 |
| `grid_cell_vectors_preserve_the_grid_element_descriptor` | `praxis-runtime/src/abi.rs` | P0-11 |
| `grid_position_vectors_use_the_point_tuple_descriptor` | `praxis-runtime/src/abi.rs` | P0-11 |
| `constructed_grid_cells_satisfy_the_declared_element_descriptor` | `praxis-runtime/src/abi.rs` | P0-11 |
| `grid_positions_vec_uses_the_point_tuple_descriptor` | `adversarial_audit.rs` | P0-11 |
| `grid_text_row_preserves_the_grid_cell_descriptor` | `adversarial_audit.rs` | P0-11 |
| `tuple_schema_uses_the_enum_descriptor_for_enum_elements` | `adversarial_audit.rs` | P0-11 |
| `tuple_schema_uses_the_unit_descriptor_for_unit_elements` | `adversarial_audit.rs` | P0-11 |
| `nested_record_inequality_dispatches_to_the_record_descriptor` | `adversarial_audit.rs` | P0-11 |
| `regression_runtime_vec_descriptor_recovers_its_real_element_type` | `praxis-debugger/src/evaluate.rs` | DBG-02 |
| `empty_vectors_with_different_element_types_are_not_equal` | `praxis-runtime/src/collections.rs` | RT-10 |
| `tuple_equality_uses_shape_not_schema_allocation_identity` | `praxis-runtime/src/tuples.rs` | RT-11 |

`tuple_schema_uses_the_unit_descriptor_for_unit_elements` needed its **program**
rewritten, not its assertion: `let unit = { let ignored = 1 }\n  (unit, 7)`
parses as a *call* of `(unit, 7)`, which is FE-04 (S12). Any test whose source
puts a parenthesized expression at the start of the line after a `let` hits
this — bind the tuple to a name and return the name.

**One exit-criterion test could not be un-ignored.**
`empty_vec_float_has_the_float_element_descriptor_before_any_push` is blocked on
**TY-08 (S13)**, not on P0-11: `let values: Vec[Float] = Vec()` never applies
its annotation to the initializer, so the element type is still a variable at
the construction site and the descriptor is legitimately null. Its `#[ignore]`
reason now says so, and it no longer *aborts the test process* on a null deref —
it reads the descriptor as an `Option`.

The eight new gates for the findings that had none:

| Test | File | Pins |
|---|---|---|
| `a_mismatched_key_never_dispatches_the_equality_callback` | `dynamic_key.rs` | RT-09 — the callback must not run at all |
| `keys_of_different_types_are_unequal_in_a_real_hash_set` | `dynamic_key.rs` | RT-09 through a real `HashSet` |
| `a_bit_outside_the_representable_range_has_no_index` | `bitset.rs` | RT-07 — the resize is unwritable |
| `a_negative_or_absurd_grid_extent_faults_instead_of_allocating` | `abi.rs` | RT-07 extents |
| `an_in_range_grid_extent_still_builds_its_cells` | `abi.rs` | RT-07's other side |
| `a_bitset_member_outside_the_representable_range_faults` | `abi.rs` | RT-07 members |
| `bitset_queries_outside_the_range_are_absent_rather_than_faults` | `abi.rs` | RT-07 — queries stay total |
| `neighbors_of_an_extreme_point_are_empty_rather_than_a_panic` | `abi.rs` | RT-07 neighbour overflow |
| `a_known_element_type_with_no_descriptor_fails_the_compile` | `lower.rs` | P0-11's D9 answer |

Plus the seven crate tests in `praxis-repr/src/tests.rs`, of which
`every_builtin_value_round_trips` is F11's stated contract: a live sample of all
twenty-one built-ins, round-tripped by descriptor **pointer**.

Two tests were **rewritten**, not merely un-ignored, because S6 inverted their
premise:

- `reset_restores_collection_pacing` grew the threshold by calling
  `collect_with`, and an explicit collection no longer grows it (RT-04). It now
  drives a real paced collection through the new test-only
  `Heap::maybe_collect_with`.
- `setting_none_cannot_create_a_pending_fault` was built on
  `fault.set(FaultKind::None)`, which no longer compiles (RT-17 made `set` take
  a `RaisedFault`). It now pins the property where it still lives —
  `RaisedFault::new` rejecting `None`, every other kind round-tripping.

Earlier, thirteen findings-without-gates got one, all checked against the
unfixed code:

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
semantics are design decision D3 and are not. See ADR-038. **This is what S10
walks into: answer D3 before writing a single `compare`.**

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

**F17 — `RootSlots`/`DebugSlots` and the `verify` pass: landed, minus two
rules.** `praxis_mir::annot` holds both sets; neither has a public constructor
taking ids, so a builder can only write `unannotated()` and
`liveness::annotate` is the only filler. `Inst::CheckFault` carries **only** a
`DebugSlots` — it allocates nothing, so it roots nothing. `praxis_mir::verify`
runs after `annotate` at all four pipeline sites plus both codegen test
harnesses. **Not done, deliberately:** `ScalarLiveAcrossSafepoint` (it fires on
every `lower_seq_*` accumulator and is harmless there — a scalar is a copy and
cannot dangle) and `OpaqueAtDescriptorSite` (H10, S15). See ADR-044 §5.
**Effect-driven safepoints** — the other third of F17 — did not land: a
safepoint is still decided by instruction shape, not by
`CallTarget::Runtime(sym).allocates()`. Nothing needed it, and the shapes agree
today.

**F18 — `Option<GcRef>` debug values and the Unit epilogue: landed whole.**
`DebugLocal.value` is an `Option<GcRef>`, niche-optimized to the same word;
`is_real_ref` and `null_sentinel_ref` are deleted from all three copies. The
fault-epilogue half (`emit_unit_return`) landed in S3.

**F16 — MirType and raw-word elimination: landed, minus `TupleShape`.**
`MirType::{Known, Opaque}` is `Local.ty`, `AllocKind::Tuple.ty` and
`AllocKind::Collection.args`; `Inst::LoadCapture` carries the capture index as
an immediate; `alloc_empty_vec` is an ordinary `AllocKind::Collection`. **Not
done:** `TupleShape` (the validated `Box<[Type]>` that makes arity < 2
unrepresentable) — the fused `enumerate`/`zip` tuples have no real type until
MIR-05 supplies one in S21, so they are `MirType::Opaque` and the backend keeps
its degenerate empty-schema path. `MirType::expect_known`/`MirTypeError` are
also not written: their only consumer is F17's verifier (S9).

**F13 — the generation arena: landed whole, and the `Generation` is the only
owner.** `praxis_codegen_cranelift::generation::Generation` is a `bumpalo::Bump`
plus the record-schema, tuple-schema, string and debug-metadata caches; a `Jit`
holds one behind an `Rc`, declared *after* `module`. Reclamation is
`Generation::retire(rc, HeapDrained)`, and `Runtime::teardown(self)` is the only
minter of a `HeapDrained`. Every allocator interns. `PlanId` is a `NonZeroU32`,
`register_plan` is bounded and fallible, and `retire_parser_plans(&proof)` drops
the plans and the interpreter's schema cache together. See ADR-043. **Not done:**
the `Generation` does not yet own the *enum* schemas (F12/RT-13 has not created
them), and `tuples::POINT` is still a process-static leak with no generation to
hang it on.

**F11 — the `praxis-repr` bridge: landed whole.** New crate, deps
`praxis-types` + `praxis-runtime` + `praxis-stdlib`, no cycle.
`descriptor_for_type` is exhaustive and fallible; `type_for_value` is its
inverse and reads payloads; `element_descriptors_for` is what a collection
constructor asks; `type_for_descriptor` is the value-less case. The payload
reads live in `praxis_runtime::repr::instance_repr`, a total match over
`BuiltinTypeId`, so the bridge never does a raw payload read itself. See
ADR-042. **Not done:** `descriptor_supports(d, cap)` — the capability-agreement
half, whose consumer is F10 (S17).

No other foundation has been started.

## 3. Things that changed under you

Mechanical consequences a fresh context will hit immediately. Items from earlier
sessions are kept — they are still true.

### From this session (S9, plus S10's RT-16)

**`Map`/`Set`/`Counter` formatting is sorted, and heaps render in pop order.**
`maps::write_sorted` is the single place the hash collections' order is
decided; it sorts *rendered entries*, which is lexicographic, not numeric
(`10` before `9`), because `TypeDescriptor::compare` is still `None`
everywhere. Any test or snapshot that depended on hash-table order was already
nondeterministic; any that now expects numeric key order will need D3 first.


**A safepoint carries two sets, and they are different types.**
`Inst::{Alloc, Materialize, Call, CallIndirect, StructEq}` have
`roots: RootSlots` **and** `debug: DebugSlots` where they had one
`live_roots: Vec<LocalId>`. `Inst::CheckFault` has `debug` and **no roots
field at all**. Adding a safepoint instruction means writing
`RootSlots::unannotated()` / `DebugSlots::unannotated()` — there is no way to
hand-write a set, which is the point: 61 literals in `build.rs` did, and
`annotate` overwrote every one.

**`emit_spill` is gone; there are `spill_roots`, `spill_debug` and
`spill_safepoint`.** The first writes the shadow frame (and *nulls* the dead
slots); the second writes the debug frame; the third is both, and is what a GC
safepoint calls. `CheckFault` calls `spill_debug` only.

**`loop_roots` is gone from the pipeline lowerings.** `invoke_closure`,
`call_predicate`, `idx_ge_len`, `emit_bounds_check`, `emit_increment`,
`idx_cmp_const`, `sink_alloc`, `emit_sink_body`, `emit_flat_map_inner`,
`run_stage` and `alloc_empty_vec` all lost the parameter. If you add a helper,
do not thread one back in.

**Root sets shrink, and shadow slots get nulled.** Any test that assumed a
value stays rooted after its last use is now wrong. Any reasoning that "the
frame holds everything ever written" is now wrong: at each safepoint the frame
holds exactly `RootSlots::live`, because `RootSlots::dead` was just zeroed.

**`DebugLocal.value` is an `Option<GcRef>`.** A bare `.as_vec()` on one no
longer compiles. `is_real_ref` is deleted from `crash_snapshot.rs`,
`render.rs` and `evaluate.rs` — match on the `Option`.

**`Inst::IntBinOp` has an `overflow: Overflow` field.** `Checked` is
source-level arithmetic; `Bounded` is a site whose operands are bounded by a
collection's length (loop index bumps, `count`, the `+ 0` scalar copy) and
emits bare arithmetic with no overflow test. `Bounded` on `Div`/`Rem` is a
verifier error.

**`MethodEntry.can_fault` is a method, not a field.** Derived from the ABI
manifest like `allocates()`. It corrected one row: `bitset.insert` declared it
could not fault while `praxis_bitset_insert` raises `InvalidSize`.

**Every host verifies.** `praxis_mir::verify(f)` runs after `annotate` in
`praxis-cli/src/run.rs`, `praxis-debugger/src/session.rs`,
`praxis-debugger/src/evaluate.rs`, and both codegen test harnesses. **If you
add MIR that breaks an invariant, the whole 382-test JIT suite fails with a
named block and instruction**, not with a mysterious segfault. Adding an
instruction now touches five exhaustive matches, not four — `verify.rs`'s
`operands` is the new one.

**`praxis_raise_empty_collection` exists** and returns the **Unit sentinel**,
not `Void`. A `Void` row would have put the context pointer in a rootable slot
(`call_symbol` hands back `ctx` for a void wrapper).

**`RUNTIME_ABI_VERSION` is 12.** Bumped for F18: the `DebugLocal` layout is
unchanged, but "no value yet" moved from `NonNull::dangling()` to the all-zero
`None`, and generated code now writes zero into a dead slot. **S9's bump is
spent — S10 starts fresh.**

### From S8

**A `Jit` owns a `Generation`, and `lower_function` takes one.** Every piece of
metadata the backend mints — record schemas, tuple schemas, field names,
function names, debug-local arrays, embedded text literals — goes into
`generation`, not into a `Box::leak`. `leak_static_str` is deleted. **Do not add
a `Box::leak` to `lower.rs`**; add a `Generation` method, where the interning
test sees it.

**`build_debug_local_metas` returns `(*const DebugLocalMeta, usize)`, not a
slice.** The array is interned by content, so two calls with identical metadata
return the same pointer.

**The record-schema cache is keyed `(GenerationId, RecordDefId)` and lives in
the generation.** A bare `RecordDefId` is a per-`TypeDb` positional index; the
process-global map it replaces is the MIR-12/DBG-06 bug. `tuple_schema_for` is
keyed by the descriptor sequence, same as before, but per generation.

**Records built in two generations no longer compare equal.** `record_equals`
compares schema *pointers*, and each generation has its own. That is RT-12 (S10)
— schema identity should be nominal or structural, not allocational. The
debugger works around it by sharing one evaluation generation.

**`Runtime::teardown(self) -> HeapDrained` exists, and it is the only proof
minter.** `Generation::retire`, `Jit::retire`,
`praxis_runtime::retire_parser_plans` and `DebugSession::teardown` all require
one. **A generation that is merely dropped leaks its arena on purpose** —
`Drop` does nothing and the `Bump` is `ManuallyDrop`. Forgetting to retire costs
memory, never soundness (hazard H15, ADR-043).

**The debugger shares one generation across every `p EXPR`.**
`DebugSession.eval_generation` is a new public field (the CLI constructs it), and
`Jit::in_generation(Rc<Generation>)` is how a throwaway module joins an existing
arena. `evaluate` and `heap` take a `&Rc<Generation>`; `type_of` does not (it
never JITs).

**`Repl::into_session` exists** and drops the snapshot before handing the
session back, which is H8's ordering made explicit rather than inherited from
field declaration order.

**A parser plan is a `PlanId`, not a `u32`, and there is no zero.**
`TypedExpr::Read`/`Parse` carry `plan: PlanId` (re-exported from
`praxis_hir::PlanId` so MIR need not depend on the input-parser crate).
`lower_to_plan` returns an owning `CompiledPlan`; `register_plan` takes it and
returns `Result<PlanId, TooManyPlans>`. The old `plan_index: 0` failure sentinel
is gone — a failed analysis lowers to `error_expr()`.

**`praxis_run_parser` validates the id it reads back.**
`crate::parser::run_plan_by_index(ctx, idx as u32, …)` is now
`run_plan_by_id(ctx, idx: i64, …)`, which does a checked `try_from` plus
`PlanId::from_raw`. Anything naming no plan is a `ParseFailed` fault.

**The runtime's parser schema caches own their storage.**
`leak_record_schema`/`leak_tuple_schema` are `record_schema_for`/
`tuple_schema_for` in `parser.rs`, backed by one `SCHEMAS` registry of boxed
entries. They must be cleared with the plans (their field names point into plan
storage), which is what `retire_parser_plans` does.

**`RUNTIME_ABI_VERSION` is still 11.** S8 changed no `#[repr(C)]` type generated
code reads: `RecordSchema`/`TupleSchema`/`DebugLocalMeta` keep their layouts, and
only the *storage* moved. **S8's ABI bump budget is unspent — S9 starts fresh.**

### From S7's second half

**There is a new crate, `praxis-repr`, and it is where descriptor questions go.**
`praxis_repr::descriptor_for_type(db, ty) -> Result<&'static TypeDescriptor,
NoRuntimeRepr>`. `lower.rs`'s own `descriptor_for_type` is now a three-line
wrapper that turns the error into an `anyhow` diagnostic; the twenty-line match
with three `_ => INT` arms is gone. **Do not add a local descriptor match
anywhere** — add an arm to the bridge, where the round-trip test sees it.

**A descriptor request can fail, and there are two kinds of failure.**
`NoReprCause::NoSuchObject` (`Never`, `UInt`, `Range`, `Seq`, a non-built-in
descriptor) is always a compile error. `NoReprCause::Unresolved` (a type
variable) is tolerated *only* where null is representable — a collection's
element descriptor — because `let xs = Vec()` generalizes at the `let` and S15
is what fixes it. Ask `e.is_unresolved()`, do not match on the reason string.

**A null element descriptor now means "unknown", and survives.**
`praxis_vec_new`/`praxis_deque_new` keep a null argument null instead of
rewriting it to `INT`. Read it through `VecPayload::element()` /
`DequePayload::element()` / `GridPayload::element()`, which return
`Option<&'static TypeDescriptor>` — **a bare deref of `element_descriptor` is
now a null deref**, and in a test that aborts the whole process rather than
failing one case. Three payloads are affected; Map/Set/Counter/heaps still hold
a non-null `&'static` and still default null to `INT` (a remaining
inconsistency, noted in ADR-042).

**`push` adopts or rejects; it never retags.** `praxis_vec_push` and both
`praxis_deque_push_*` call `adopt_or_reject`, which sets the descriptor if there
is none, accepts if it matches by pointer, and raises `FaultKind::TypeMismatch`
otherwise **without storing the value**. `praxis_grid_set` does the same.

**`FaultKind` gained `InvalidSize` and `TypeMismatch`** — a `match` over it is
two arms longer again (four since S6). Neither needed an ABI bump: generated
code never switches on a fault kind and the `#[repr(C)]` enum's width is
unchanged. **`RUNTIME_ABI_VERSION` is still 11.**

**Grid results carry real descriptors.** `cells`/`row`/`column` are tagged with
the grid's cell descriptor; `positions`/`neighbors4`/`neighbors8`/`find_all`
with `tuples::TUPLE`. `praxis_grid_new` fills cells with the *zero value of the
cell type* (`default_cell`, an exhaustive match over `BuiltinTypeId`) and raises
`TypeMismatch` for a composite cell type it cannot invent a value for.

**Collection equality compares element descriptors, and tuple equality compares
shape.** `same_element` (pointer identity, two nulls agree) leads
`vec_equals`/`deque_equals`/`grid_equals`. `TupleSchema::same_shape` replaces
the schema-*address* comparison, and `tuple_hash` now mixes each slot's
descriptor id so `Eq` and `Hash` still agree.

**`DynamicKey`'s fields are private.** Use `k.value()` and `k.descriptor()`.
Equality short-circuits on descriptor pointer identity before touching a
callback.

**`GridExtent` and `BitIndex` are the only routes from an `Int` to a size.**
`BitSetPayload::{insert, contains, remove}` take a `BitIndex`, not a `usize`.
Both cap (`MAX_CELLS = 2^28`, `BitIndex::MAX = 2^32 - 1`) rather than merely
checking overflow. See ADR-041.

**The debugger reads types out of values, not out of descriptors.**
`descriptor_to_type`/`descriptor_id_to_type` are deleted;
`collect_bindings` calls `praxis_repr::type_for_value`. `p xs.get(0)` on a
`Vec[Text]` now types as `Text`.

### From S6 and the first half of S7

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
unchanged. **S7's one bump (H17) is spent, and S7 is closed — S8 starts with a
fresh one.**

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

**S10, continued** (P0-12, RT-12; DBG-01 and DBG-02 need nothing more here,
**RT-16 is done**). S9 is closed; every finding it owned is fixed and gated.

RT-16 was taken first because it is the one S10 finding that needs nothing from
D3. What remains does.

- **D3 blocks P0-12 and nothing else in the stage can substitute for it.**
  `TypeDescriptor::compare` exists on all 21 descriptors and is `None`
  everywhere (F1, ADR-038). Populating one needs an answer to: NaN ordering, and
  whether Text / tuples / records / collections are orderable at all. **Answer
  D3 first** — the ordering tests the plan lists as exit criteria all assert a
  semantics nobody has chosen. See §5.
- **RT-12 is the one S9 made louder.** Records built in two generations compare
  unequal, because `record_equals` compares schema *pointers* and each
  generation has its own (S8, §3). The debugger works around it by sharing one
  evaluation generation. RT-12 replaces allocational identity with nominal-or-
  structural identity; the plan's new tests are "two anonymous schemas with the
  same shape compare equal, two nominal records with different defs do not".
- **Land the `compare` field before RT-13's `EnumSchema` work in S18**, or the
  same 21 initializers conflict twice. That is the plan's own sequencing note.
- **DBG-01 is already closed** (`3836b74`) and **DBG-02 is closed for values**
  (S7's `type_for_value`). What remains of DBG-02 is the *value-less* half: a
  record or enum object records no nominal type (F12) and a closure records no
  signature. See §6.
- **S10's ABI bump budget is unspent** (H17). `RUNTIME_ABI_VERSION` is 12, and
  RT-16 changed no `#[repr(C)]` type. Adding `compare` to `TypeDescriptor`
  needs **no** bump on its own either — generated code passes descriptors by
  pointer without reading their fields (the same reasoning that made S6's
  second half and S8 free). Check what generated code actually reads before
  spending it.
- **RT-16 sorts by the *rendered* entry, and that is D3's debt.** `Map`/`Set`/
  `Counter` formatting renders each entry, sorts the strings, and joins, because
  a rendered string is the only total order available while
  `TypeDescriptor::compare` is `None`. So `{10: a, 9: b}` prints `10` first.
  `maps::write_sorted` is the one place that changes when D3 lands. Heaps did
  not need it — they carry an ordering by construction and render in pop order.

**What S9 deliberately left:**

- **`ScalarLiveAcrossSafepoint` is not a verifier rule**, and ADR-044 §5
  records why: it fires on every `lower_seq_*` accumulator by construction and
  is harmless there, since a scalar is a copy of a payload and cannot dangle.
  Do not add it without moving those accumulators into `Gc` slots first.
- **`OpaqueAtDescriptorSite` stays off until S15** (H10), and
  **`MirType::expect_known`/`MirTypeError` are still unwritten** — the plan
  puts them in S9 because F17's verifier is their only consumer, but that
  consumer is the rule H10 defers. They land in S15, together.
- **Effect-driven safepoints did not land.** F17's third part wanted
  `CallTarget::Runtime(sym).allocates()` to decide what is a safepoint;
  instruction shape still decides. The two agree today, and nothing needed the
  change. A `Pure` runtime call still gets a root set it does not need.
- **`v.sum()` overflow is still observed late.** The accumulator is
  `Overflow::Checked`, but no `CheckFault` follows the loop, so the fault is
  sticky and the host sees it after `main` returns instead of unwinding at the
  sink. This is the residue of P0-08's open question; the verifier has no
  "every faulting instruction is observed" rule because the codebase does not
  satisfy one yet.
- **`min`/`max` on an empty sequence still return `0`.** They share MIR-09's
  empty case but not its defect — the accumulator is a scalar initialized to
  `0`, so the answer is defined, if debatable. `adv_pipeline_empty_source_min_is_zero`
  pins it. Whether `0` is right is **D1**'s question, and S18 is where it gets
  settled alongside `Map.get` / `Grid.find`.

**What S8 deliberately left** (still true):

- **`tuples::POINT` is still a process-static leak.** One `TupleSchema` for every
  grid position, minted by the runtime rather than by a compile, so there is no
  generation to hang it on. Bounded at one.
- **Enum schemas have no generation home** because they do not exist yet
  (RT-13/F12, S18). When they do, they belong in `Generation` beside the record
  and tuple caches — the plan sketch already lists an `enum_schemas` field.
- **A plan is registered per compile and never deduplicated.** A debugger session
  that reloads a thousand times registers a thousand plans; `MAX_PLANS` (2^20)
  catches a runaway, and `retire_parser_plans` reclaims at teardown, but there is
  no interning as there is for JIT metadata. Keying on the `ParserAst` would give
  it; nothing needs it yet.

Re-read §6 of the plan first. The hazards that still bind: **H17**, and **H10**
in its long form (the MIR verifier's "no `Opaque` in a descriptor-producing
position" rule stays off until S15). **H3 is discharged** — the debug/root
split landed first, the two m11 tests are green, and
`the_debug_set_still_shows_what_the_root_set_dropped` now states the property
at the level it lives at rather than leaving it to a CLI snapshot three layers
away. **H15 is discharged** — ADR-043 encodes the ordering in `HeapDrained`
rather than documenting it. **H1, H2, H4, H6, H7, H8, H9 and H16 remain
discharged.**

## 5. Design decisions still open

**D14 is answered** — see ADR-040. The `Safepoint` token shipped with a named,
`pub(crate)` `Heap::alloc_unpaced` back door for the host helpers and the
parser interpreter (the plan's option 2), rather than landing IPR-14 out of
stage order (option 1) or deferring the token (option 3).

**D9 is answered** — see ADR-042. The JIT **fails the compile with a
diagnostic** naming the type and the site, which is the option the plan's own S7
text annotates "(correct)". Two carve-outs where absence is already
representable and already rendered: debug metadata emits a null descriptor plus
`NO_STATIC_TYPE` (what `MirType::Opaque` locals already do), and a collection's
element descriptor may be null. A third distinction fell out of the work and is
part of the answer: an *unresolved type variable* is not the same failure as a
type that can have no object, and only the first is tolerated — see
`NoReprCause` and hazard H10.

**D12 (panic-across-FFI) remains open** and RT-06/RT-07 landed without it. What
shipped is narrower than a policy: the two wrappers the audit named now fault
instead of aborting. Other wrappers still reach Rust panics on malformed input.

**D1, D3 and D5 still block their stages**; none has been answered. **D3 is the
next one that binds — S10 cannot start P0-12 without it.**

**D1 gained a second case.** It was scoped to `Map.get` / `Grid.find`; the same
question is now also open for `min`/`max` on an empty sequence, which return
`0` because their accumulator is a scalar seeded to `0`. MIR-09 gave the three
*seeded* sinks (`reduce`, `min_by`, `max_by`) a fault, because their answer was
genuinely undefined; `min`/`max` have a defined answer that may be the wrong
one. Settle both together.

| | Decision | Blocks |
|---|---|---|
| D3 | NaN ordering, and whether Text/tuples/records/collections are orderable at all. **This is what `TypeDescriptor::compare` is waiting for** | S10, blocking P0-12 |
| D7 | After `_` lexes as `UNDERSCORE`, is it still legal in `let _ = f()`, `fn g(_)`, `\|_\| 0`? | S12 |
| D8 | Exactly where a newline terminates an expression | S12 |
| D13 | Diagnostic-code allocation for the whole block, before S13 starts | S13/S16 |
| D2 | Loop break-value semantics | S14 |
| D4 | Hashability of mutable collections as Map keys | S17 |
| D5 | The 15 phantom prelude names: implement or delete | S17 |
| D6 | `CollectionCtor::Range`: delete or implement | S17 |
| D1 | `Map.get` / `Grid.find` — `Option[V]` or V-with-Unit; and `min`/`max` on an empty sequence. Source-visible | S18 |
| D10 | How much parser-expression grammar a template capture body may contain | S19 |
| D11 | `grid(int)` granularity; greediness of `text`/`word` | S20 |
| D12 | Panic-across-FFI policy — should precede RT-06, RT-07 and the parser findings | cross-cutting |

## 6. Corrections to the plan

Things the plan states that are no longer or were not quite true.

- **F17's `RootSlots` sketch is one field short.** `unannotated()` / `iter()` /
  `is_annotated()` / `set()` describe the *live* set only, but MIR-01 needs a
  second one: which slots to **null**. Nulling every non-root slot at every
  safepoint would cost `gc_count` stores per safepoint, so `RootSlots` carries
  `live` and `dead`, and `dead` comes from a forward may-dataflow over "which
  slots might still hold a value". The plan describes MIR-01 as "dead-slot
  clearing" without saying where the list comes from; it is a third analysis,
  not a filter on the second.
- **F17's `verify` cannot have all the rules it lists, and two of the absences
  are decisions.** `ScalarLiveAcrossSafepoint` fires on every `lower_seq_*`
  accumulator — F17 predicts this and asks for an explicit decision, which is
  ADR-044 §5: the rule is not implemented, because a scalar is a *copy* of a
  payload and cannot dangle, so the invariant that matters is already stated by
  `RootIsNotGc` + `MoveGcFromScalar`. `MissingTerminator` is unrepresentable —
  `Block.term` is not an `Option` — so there is no rule rather than a heuristic
  about placeholder self-jumps.
- **F17's "effect-driven safepoints" did not land, and nothing needed them.**
  The `Inst` shape still decides what is a safepoint;
  `CallTarget::Runtime(sym).allocates()` is not consulted. The two agree at
  every site today. `AllocatingCallNotASafepoint` therefore has no rule.
- **`MirType::expect_known`/`MirTypeError` do not land in S9.** The plan puts
  them here because F17's verifier is their only consumer — but that consumer
  is `OpaqueAtDescriptorSite`, which H10 defers to S15. Writing the API without
  its rule would be adding unused surface. They land in S15.
- **The plan says "two loop-increment `IntBinOp` sites"; there are eight
  compiler-written sites**, and they split three ways. Three loop index bumps,
  two `count` accumulators and one `+ 0` scalar copy are bounded by a
  collection's length and are now `Overflow::Bounded`. Two are `sum`/`product`
  accumulators, which *can* genuinely overflow and are `Checked` — and are still
  not followed by a `CheckFault`, so the fault is observed after `main` returns
  rather than at the sink. That residue is why the verifier has no "every
  faulting instruction is observed" rule.
- **MIR-01 needed the accumulator initialization MIR-09 brought, not just the
  clears.** `reduce`/`min_by`/`max_by` left a `Gc` slot unwritten on the empty
  path, and liveness roots it at the loop header — so the backend spilled an
  *undefined Cranelift value* into the shadow frame for the collector to
  dereference, on every empty `reduce`, whether or not anyone read the result.
  The audit describes MIR-09 as a bad return value; it was also a live rooting
  bug.
- **MIR-09's raise wrapper cannot be `Void`.** `call_symbol` hands back the
  *context pointer* for a void wrapper (a deliberate simplification), so an
  `Inst::Call` to one writes `ctx` into its `Gc` destination — the exact class
  of bug this stage removes. `praxis_raise_empty_collection` is
  `(Ctx) -> Gc, Faults` and returns the Unit sentinel. H16's "MIR-09 adds
  `praxis_raise_empty_collection`" is otherwise right, and it really was two
  edits.
- **S9's ABI bump is for a layout that did not change.** `DebugLocal.value`
  went from `GcRef` to `Option<GcRef>`, which is the same word at the same
  offset. What changed is the *meaning*: "no value yet" moved from
  `NonNull::dangling()` to the all-zero `None`, and generated code now writes
  zero into a dead shadow slot. A previous-version runtime reads either zero as
  a reference. The rule "check what generated code actually reads" cuts both
  ways — a stable layout is not automatically a free stage.
- **F18's `DebugLocalMeta` sketch is already what shipped**, in S6: the
  `NO_STATIC_TYPE` half landed with `MirType::Opaque`, not here. Only the
  `DebugLocal.value` half was S9's, and the fault-epilogue half was S3's.
- **F13's `Generation::retire(self, HeapDrained)` signature could not be
  written as sketched.** A `Jit` holds the generation behind an `Rc` (the
  debugger shares one across every `p EXPR`), so it is
  `Generation::retire(Rc<Generation>, HeapDrained)` and a still-shared
  generation is left alone rather than freed. The plan's `#[must_use]` is also
  not what landed: Rust has no linear types, so the enforcement is that
  `Generation::drop` *leaks*. Reclaiming needs the proof; forgetting to reclaim
  costs memory, not soundness.
- **F13 says "every `Box::leak` becomes `gen.alloc*`". Reclaiming is not enough
  on its own.** A debugger session never ends, so DBG-05's "must not grow
  without bound" needs *interning*, and needs the `p` path to share one
  generation rather than mint one per command. Both landed; the plan sketch
  mentions neither.
- **F13's `PlanArena` cannot live in the codegen generation.** Plans are
  registered during HIR lowering, before a `Jit` exists. It is a process-wide
  bounded arena in `praxis-input-parser` instead, retired through
  `praxis_runtime::retire_parser_plans` — the proof has to be applied from
  `praxis-runtime` because `praxis-input-parser` cannot depend on it (the
  interpreter points the other way).
- **F13's "39 workspace-wide" leak count included test helpers.** The production
  sites S8 owed are all converted; what remains under `Box::leak` in non-test
  code is `tuples::POINT` (one process-static schema, bounded at one) and the
  `Box::leak(Box::new(rt.context()))` idiom in in-crate tests.
- **S8's exit criterion "leak_static_str must no longer exist" is structural.**
  It is deleted, and there is no test for it because there is nothing left to
  test — its callers take a `&Generation`.
- **S8 needed no ABI bump.** `RecordSchema`, `TupleSchema` and `DebugLocalMeta`
  keep their `#[repr(C)]` layouts; only their storage moved. Check what
  generated code actually reads before spending a stage's bump.
- **DBG-01 is closed** (`3836b74`), not open in S10. **DBG-02 is closed for
  values**: the debugger reads a value's real type out of its payload through
  F11's `type_for_value`, and its gate
  `regression_runtime_vec_descriptor_recovers_its_real_element_type` is
  un-ignored. What remains of DBG-02 in S10 is the *value-less* half — a record
  or enum object does not record which nominal type it is (F12), and a closure
  records no signature. Both report why rather than guessing.
- **S7's exit criteria list one test that S7 cannot pass.**
  `empty_vec_float_has_the_float_element_descriptor_before_any_push` needs the
  `let values: Vec[Float] = Vec()` annotation to reach the initializer, which is
  TY-08 in **S13**. P0-11 makes the descriptor honestly *null* instead of
  wrongly `Int`; it cannot make it `Float`.
- **P0-11's "expect passing tests to flip" was half right.**
  `vec_float_push_adopts_float_descriptor_and_preserves_signed_zero_semantics`
  (adversarial_audit.rs, the test H18 names) **still passes unchanged**, because
  its `let a = Vec()` genuinely has no static element type and adopt-on-first-push
  is the honest answer there. What P0-11 removed was *retagging* a vector that
  had been told its type. The test that actually needed rewriting was
  `tuple_schema_uses_the_unit_descriptor_for_unit_elements`, and for an unrelated
  reason (FE-04, see §1).
- **A test that derefs a payload pointer can abort the whole test binary.**
  `empty_vec_float_…` did, once the descriptor became legitimately null: a null
  deref in Rust is a non-unwinding panic, so one bad assumption took out
  thirty-six other tests in the same process with SIGABRT and no failure list.
  Plan §8.1's "do not batch the ignored suite" is about this class; the cheap
  defence is to read through the `Option` accessor.
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
- **`MethodEntry.can_fault` is dead metadata.** Nothing reads it — every method
  call emits an unconditional `check_fault` in `build.rs`. That is why RT-07's
  and P0-11's new faults are observed by generated code without touching a
  lowering, and it is also why the field's `false` values (`bitset_insert`,
  `vec_push`) were never wrong in a way anyone noticed. F17's verifier (S9) is
  where "faulting instruction ⇒ CheckFault" becomes a real rule; either wire
  `can_fault` to the manifest's `Effect` there or delete it.
- **Adding a `FaultKind` variant needs no ABI bump.** Generated code never
  switches on the kind — it calls `praxis_check_fault`, which answers a bool —
  and the `#[repr(C)]` enum's width does not change. Four variants have been
  added across S7 (`InvalidChar`, `InvalidText`, `InvalidSize`, `TypeMismatch`)
  under one bump, which was spent on the `Fault` *repack*, not on the kinds.
