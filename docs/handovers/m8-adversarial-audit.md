# M8 adversarial test audit — findings & handover

**Date:** 2026-07-26

> **Follow-up (2026-07-26):** all five leftover issues documented below have been
> addressed in four commits on `main`. §2 (closure-from-collection SIGSEGV),
> §6.2 (deep-recursion SIGABRT), §6.3 (read-against-non-Text SIGSEGV), and the
> §6.1 schema-cache residual are **FIXED**. §3 Gap A (fold-into-Vec) is **FIXED**;
> §3 Gap B is **partially fixed** (closure-param element-type propagation is now
> implemented; the residual blocker is the orthogonal HM let-generalization of
> `Vec()` bindings, documented in the test). See the "Status" lines in each
> section and the new tests referenced there. The JIT crate grew 313 → 321
> tests; `cargo fmt --check` + `cargo clippy -D warnings` + `cargo test
> --workspace` all clean.
**Scope:** An adversary-style audit of the M8-WS11 pipeline fusion, closures,
GC, runtime collections, arithmetic, and input parser. Two rounds of ~108 new
adversarial end-to-end JIT tests were added to
`crates/praxis-codegen-cranelift/tests/jit.rs` (all `adv_*`), probing edge
cases the existing suite skipped: mutable captures mutated inside fused loops
under GC pressure, nested closure allocation during fusion, fault propagation
through every fused sink/stage, take(0)/skip edges, object-valued pipeline
elements, closure/VarCell/shadowing interactions, arithmetic overflow corners
(MIN/-1, modulo sign, negation), Map/Set/Counter with distinct-allocation and
source-slice Text keys, deep recursion, parser-built records with non-Int
fields, grid round-trips, and CSV at non-zero offsets.

**Result:** three real bugs found — **two fixed** (flat_map, parser-record
descriptors), **one documented** (deep-recursion stack overflow). Plus several
documented limitations. The fused-loop GC rooting, the overflow/division guards,
and the structural Text-key hashing all held up under dedicated stress.

---

## 1. BUG (fixed): `flat_map` bypassed subsequent pipeline stages

**Severity:** correctness — silent wrong answer.
**Status:** **FIXED** in `crates/praxis-mir/src/build.rs`.

### Symptom

`v.flat_map(f).filter(p).sum()` returned the sum of **all** flat-mapped
elements, ignoring the filter. Reproduction:

```praxis
// [1,2].flat_map(|x|[x,x*10]) = [1,10,2,20].filter(>5) should be [10,20].sum()=30
// BUG: returned 33 (1+10+2+20 — filter never applied)
```

### Root cause

In `lower_pipeline`, the `Stage::FlatMap` arm ran `emit_flat_map_inner`, which
called `emit_sink_body` **directly** on each inner element, then set
`alive = false` — skipping every stage after the flat_map. So
`flat_map(...).filter(...).sum()` compiled to `flat_map(...).sum()`. The
existing WS11 tests (`pipeline_flat_map_sum`, `pipeline_flat_map_collect_len`)
only placed flat_map as the *last* stage before the sink, so they never caught
this.

### Fix

`emit_flat_map_inner` now takes the slice of **remaining** stages (those after
flat_map in the chain) and runs them — via the same `run_stage` helper the outer
loop uses — on each inner element before the sink. The inner loop has its own
`header / body / inner_incr / inner_exit` blocks with a nested `LoopCtx` so a
post-flat_map `filter`'s skip jumps to the inner increment (not the header,
which would spin — the same M8-WS11 infinite-loop bug, guarded against), and a
`take`/`take_while` break jumps to the inner exit.

The `find`/`position` sinks now report the **inner** index (position within the
flat_map's output), matching eager semantics where flat_map produces a flat
sequence.

### Regression test

`adv_pipeline_flat_map_with_filter_then_sum` (and `adv_pipeline_empty_flat_map_
yields_empty`, `adv_pipeline_flat_map_gc_stress_preserves_inner_vecs`) cover
the fixed path. Full suite green (676 → 719 tests; the JIT crate 205 → 263).

---

## 2. ~~KNOWN LIMITATION (unfixed)~~ FIXED: invoking a closure retrieved from a collection

**Severity:** memory-safety / miscompile — **can SIGSEGV**.
**Where:** parser + HIR call lowering.
**Status:** **FIXED** (commit `7aaa1c8`). A postfix `expr(args)` parse
production, an AST `callee_expr()` accessor, resolver/HIR/infer handling of the
expression callee, and an MIR `Inst::CallIndirect` lowering together make
`fs.get(0)(100)`, `f(1)(2)`, and `(|x| x*3)(14)` work end-to-end. Regression
tests: `adv_call_closure_retrieved_from_collection`,
`adv_call_closure_in_parens`, `adv_call_result_of_call`,
`adv_call_closure_from_collection_under_gc_pressure`.

### Symptom

Calling a closure value obtained from anything other than a local binding
miscompiles and can crash the host:

```praxis
let fs = Vec()
fs.push(|x| x + 1)
fs.get(0)(100)   // ← MISCOMPILES: produces garbage, may SIGSEGV
```

### Root cause

The call-lowering dispatch (`lower_expr_gc`'s `TypedExpr::Call` arm,
`crates/praxis-mir/src/build.rs` ~line 715) decides direct-vs-indirect call by
checking `b.locals.get(callee)`. A local-bound closure resolves to a local →
indirect call (correct). But a callee that is a method-call result
(`fs.get(0)`) resolves to **no symbol** (`SymbolId(u32::MAX)`, empty
`callee_name`), so it falls through to the **direct-call** path
(`CallTarget::User("")`), which is nonsense — it does not read the closure
value's `fn_ptr` and call through it.

Worse, this is not caught at parse/typecheck: `CallExpr::callee()` is typed as
`Option<PathExpr>` (`crates/praxis-ast/src/nodes.rs:1163`) — the AST only
represents a *named* callee, not an arbitrary expression callee. The parser's
postfix loop (`crates/praxis-parser/src/parse.rs:1142`) only handles `.method()`
(DOT); there is no postfix `(args)` production. So `fs.get(0)(100)` parses into
something that typechecks (no diagnostic) but lowers to a garbage direct call.

### What a fix needs

1. **Parser:** a postfix call production so `expr(args)` is parsed with the
   callee being the preceding expression (not just a `PathExpr`).
2. **AST:** `CallExpr::callee()` should yield an `Expr`, not just a `PathExpr`.
3. **HIR `lower_call`:** when the callee is a non-path expression, lower it to
   a GcRef local and emit `Inst::CallIndirect` (read `fn_ptr`, `call_indirect`).
4. **Typecheck:** the callee expression's type must be a function/closure type.

Until then, the supported path is: bind the closure to a local first
(`let f = fs.get(0); f(100)` works). The broken pattern is **not** tested in
CI (it is non-deterministically a SIGSEGV, which would flake); a comment in
`adv_indirect_call_on_local_closure_works` documents the boundary.

---

## 3. ~~KNOWN LIMITATION (unfixed)~~ FIXED (Gap A) / PARTIAL (Gap B): type inference gaps blocking pipelines

Bidirectional (expected-type) inference was added to `infer_method_call`
(commit `025721e`): the method's full signature (receiver + params + result) is
instantiated name-aware (one type var per `Var(name)`, so fold's `Acc` is a
single type across the init arg, closure params, and result), the receiver is
unified against its pattern to pin the element type, and each argument is
inferred with its expected param type pushed down — unifying immediately so a
shared var propagates to later args. A closure arg whose expected type is a
`Func` gets each param unified with the expected Func param before its body is
inferred. This is purely additive (unifying with a fresh var is a no-op), so all
86 existing pipeline tests keep their behavior.

- **Gap A — FIXED.** `adv_pipeline_fold_into_vec_unsupported_by_inference` →
  renamed `adv_pipeline_fold_into_vec_now_supported`: a positive runtime test.
  `v.fold(Vec(), |a, x| { a.push(x); a })` now type-checks and runs; the closure
  param `a` is pinned to the init arg's accumulator type (threaded via the
  name-shared `Acc`).
- **Gap B — PARTIALLY FIXED.** The closure-param element-type propagation this
  section described is now implemented. The idiomatic
  `let v = Vec(); v.push(...); v.map(|inner| inner.len())` pattern is STILL
  rejected, but for a DIFFERENT, orthogonal reason: HM let-generalization turns
  `let v = Vec()` into `forall T. Vec[T]`, so each method call on `v` instantiates
  a fresh, unbound element type and the push that would pin it doesn't propagate
  to the map. That is a generalization-policy issue for mutable Vec bindings, not
  the closure-param gap targeted here.
  `adv_pipeline_method_on_closure_param_partially_supported` documents the
  residual (clean diagnostic, not a crash).

---

## 4. What held up (the GC-rooting is sound)

These dedicated stress tests pass, confirming the ADR-029 claim that the
liveness pass recomputes correct roots across fused stages:

- `adv_pipeline_mutable_capture_mutated_in_fused_loop_gc_stress` — a `var`
  captured by a map closure, mutated every iteration, 300 elements. VarCell
  survives GC; final read correct (44850).
- `adv_pipeline_collect_vec_elements_survive_gc_stress` — collect under GC
  pressure, then read back in a separate loop (134550).
- `adv_pipeline_fold_accumulator_is_gc_int_under_pressure` — fold acc threaded
  across 500 iterations (124750).
- `adv_pipeline_nested_closure_allocation_gc_stress` — map closure *returns* a
  closure (allocating env each iteration), 200 elements collected.
- `adv_pipeline_flat_map_gc_stress_preserves_inner_vecs` — flat_map inner Vecs
  survive the inner loop's GC (14850).
- `adv_pipeline_min_by_under_gc_pressure` / `max_by` / `min` — running-best
  GcRef survives 500-element GC pressure.
- `adv_fused_*_fault_propagates` (5 tests) — div-by-zero / overflow in map /
  filter / fold / find / sum closures propagates as the right `FaultKind`
  without crashing the host.

---

## 5. Minor notes

- **`|| a` parses as logical-or**, not a zero-arg closure. The existing
  `capture.rs` test already notes this; it's an ergonomic gotcha. Zero-arg
  closures need a dummy `_` param (`|_| a`) today. Not a bug, but undocumented
  in the spec.
- **`min`/`max`/`reduce` on an empty source** return 0 / 0 / undefined (the
  seen-flag never trips). `adv_pipeline_empty_source_*` tests document the
  current behavior. A future `Option` return (per ADR-029's `find`/`position`
  note) would be the principled fix.
- **`take`/`skip` with negative literals** behave per the `idx >= n` / `idx < n`
  guards: `take(-1)` stops immediately (0 ≥ -1), `skip(-1)` keeps all
  (idx < -1 is false). Reasonable; documented by `adv_pipeline_take_zero` /
  `skip_zero` / `skip_more_than_length`.

---

## 6. Round 2: arithmetic, collections, parser, GC — additional findings

A second adversarial pass beyond pipelines/closures found two more bugs and
confirmed several subsystems are sound.

### 6.1 BUG (fixed): parser records with non-Int fields SIGSEGV'd

**Severity:** memory-safety — **SIGSEGV** from valid input.
**Status:** **FIXED** in `crates/praxis-runtime/src/parser.rs::alloc_record`.

`alloc_record` hardcoded every field's descriptor to `scalars::INT`
(parser.rs:568) with a comment claiming the per-field descriptor "is read from
the value's own header at trace/format/eq/hash time." **That comment was
wrong**: `record_equals`/`record_format`/`record_hash` (records.rs) all dispatch
through `schema.fields[i].descriptor`. So a parser-built record with a Text
field (e.g. `read lines(`{name:word},{port:int}`)`) compared, hashed, or
formatted its Text field through INT's callback — reinterpreting the
`TextPayload` struct bytes as an `i64`, then dereferencing the garbage →
SIGSEGV.

Reproduction (pre-fix): `read lines(`{name:word},{port:int}`)` then `a == b`
or using the record as a `Set` key both segfaulted.

Fix: `alloc_record` now records `value.descriptor()` for each field (the
capture value's real type). 4 regression tests
(`adv_parser_record_with_text_field_*`) cover equality, set-key, and
GC-survival of Text-field parser records.

**Residual note (FIXED):** `leak_record_schema` previously cached schemas by
field-*name* sequence only, so two templates with the same field names but
different capture types (e.g. `{x:word}` vs `{x:char}`) would collide and share
the first-seen schema's descriptors — the same class of segfault the
`alloc_record` fix above closed. **Fixed** (commit `23130ee`): the cache is now
keyed on `(names, descriptor-pointers)`, mirroring the sibling
`leak_tuple_schema`. Regression tests: `adv_parser_record_same_name_diff_type_
no_schema_collision`, `adv_parser_record_same_name_diff_type_survives_gc`.

### 6.2 BUG (fixed): deep recursion aborts the host (SIGABRT)

**Severity:** robustness — kills the process; violates §9.2/§17.4.
**Status:** **FIXED** (commit `cd61ad3`). A new `FaultKind::StackOverflow` and a
`recursion_depth` counter on `RuntimeContext` (bumped in
`praxis_push_shadow_frame`, decremented in `praxis_pop_shadow_frame`) back a
prologue guard in every generated function: after the shadow-frame push, read
`recursion_depth` at its fixed `#[repr(C)]` offset and branch — if it exceeds
`MAX_RECURSION_DEPTH` (8000), to a stack-overflow fault epilogue (raise the
fault, pop frame, return the Unit sentinel); else to the body.

The enabling change: `Inst::CheckFault` now actually **branches** to the
function's fault block when a fault is pending (previously a no-op
`praxis_check_fault` call — the "full per-check branching is a follow-up" TODO).
Without it, a `StackOverflow` set deep in recursion would return Unit to the
parent, which would feed Unit into an arithmetic wrapper and dereference Unit's
payload as an `i64` (UB → segfault). Branching at every `CheckFault` diverts to
the fault block before any such operand is touched, so the fault unwinds
cleanly through every parent frame. This also hardens all other fault kinds.
Regression tests: `adv_deep_recursion_does_not_crash_host` (count(4000) under
the limit — succeeds), `adv_deep_recursion_over_limit_faults_cleanly`
(count(100000) over the limit — pre-fix SIGABRT, now `FaultKind::StackOverflow`,
host survives).

### 6.3 Host-safety gap (fixed): `read` with no input buffer

The test harness `run_main_with_input` previously skipped installing an input
buffer for empty input, leaving `ctx.input_source` at its default (the Unit
singleton). A `read` against Unit reinterprets Unit's payload as a Text
buffer → SIGSEGV. The harness always installs a (possibly empty) Text buffer.

**The underlying runtime gap is FIXED** (commit `23130ee`): `praxis_run_parser`
(the ABI chokepoint both `read` and `parse(text, expr)` funnel through) now
guards the input descriptor — if it is not `TEXT`, it raises `ParseFailed` and
returns the Unit sentinel instead of handing the parser interpreter a non-Text
payload to reinterpret. The host observes the fault cleanly. Regression test:
`adv_read_against_non_text_input_faults_cleanly` (+ a `run_main_no_input` helper
that deliberately leaves `input_source` at the default Unit).

### 6.4 What held up (round 2)

- **Arithmetic (§4.12):** all overflow/division guards are correct end-to-end
  through the JIT — `MIN / -1` and `MIN % -1` (the SIGFPE trap on x86) fault
  cleanly as `IntOverflow`; modulo sign follows dividend (truncate-toward-zero);
  negation of MIN overflows; compound-assign (`+=`, `*=`) and loop-accumulator
  overflow all fault. 15 tests.
- **Map/Set/Counter Text keys (the handover's "known bug"):** NOT reproduced —
  distinct-allocation and source-slice (`read`) Text keys aggregate and look up
  correctly through `DynamicKey`'s structural eq/hash path. 14 tests.
- **Large collections under GC:** 500-entry Map/Set/Counter, 30×30 Grid, 500-bit
  BitSet, 200-entry MinHeap all survive GC pressure with correct reads.
- **Grid round-trips:** rotate×4 = identity, transpose×2 = identity.
- **Nested composite equality:** `Vec[Vec[Vec[Int]]]`, tuples-of-records compare
  correctly (equal and unequal).
- **Fault state:** two faults in sequence (separate `Runtime`s) start clean; OOB
  and negative-index vec access fault as `IndexOutOfBounds`.

### 6.5 Confirmed inference gaps (round 2)

The `m[key]`-vs-`.get` distinction (§4.7) is not implemented: `m["missing"]`
returns Unit (same as `.get`) rather than faulting. Documented by
`adv_map_index_missing_key_does_not_fault_current_behavior`.

---

## 7. Test inventory added

All in `crates/praxis-codegen-cranelift/tests/jit.rs`, prefixed `adv_`:

- Pipeline fusion edges: `take_zero`, `skip_zero`, `skip_more_than_length`,
  `take_then_skip_then_map_sum`, `count_after_filter_all_dropped`,
  `empty_source_*` (collect/min/any/all/reduce), `map_filter_map_filter_sum_
  deep_chain`, `two_chains_share_no_state`, `sum_does_not_mutate_source_vec`,
  `chained_collect_used_as_receiver_of_next_chain`.
- GC stress: `mutable_capture_mutated_*`, `collect_vec_elements_survives_*`,
  `fold_accumulator_is_gc_int_*`, `reduce_into_int_accumulator`,
  `nested_closure_allocation_*`, `nested_vec_elements_survive_fused_count`,
  `flat_map_gc_stress_*`, `min/max(_by)_under_gc_pressure`, `zip_under_gc_
  pressure`, `take_while_then_collect_*`, `take_then_count_*`.
- flat_map (the fix): `flat_map_with_filter_then_sum`, `empty_flat_map`.
- Fault propagation: `fused_sum_overflow_faults_cleanly`, `fused_map_closure_
  fault_propagates`, `fused_filter_predicate_fault_propagates`, `fused_fold_
  closure_fault_propagates`, `fused_find_predicate_fault_propagates`.
- Closures/captures: `nested_closures_share_var_cell*`, `closure_mutating_
  capture_then_returned_*`, `recursive_function_with_captured_var`,
  `curried_closure_used_in_pipeline_gc_stress`, `mutable_capture_*_in_fused_*`,
  `shadowing_then_closure_captures_correct_binding_*`, `indirect_call_on_local_
  closure_works`.
- Inference-limitation assertions: `fold_into_vec_unsupported_by_inference`,
  `method_on_closure_param_from_collection_rejected`.
- Arithmetic edges (§4.12): `int_min_div_neg_one_overflows`,
  `int_min_mod_neg_one_overflows`, `modulo_by_zero_faults`,
  `modulo_negative_operands_truncates_toward_zero`,
  `modulo_positive_dividend_negative_divisor`, `division_truncates_toward_zero`,
  `int_min_minus_one_overflows`, `int_max_times_two_overflows`,
  `int_min_times_neg_one_overflows`, `negate_int_min_overflows`,
  `compound_add_assign_overflow_faults`, `compound_mul_assign_overflow_faults`,
  `loop_accumulator_overflow_faults`, `max_plus_zero_is_max`, `div_normal_case`.
- Map/Set/Counter (distinct-alloc & source-slice keys): `counter_text_keys_from_
  vec_accumulate`, `counter_text_keys_from_read_*`, `map_text_key_*`,
  `set_text_key_*`, `set_dedupes_distinct_alloc_equal_text`,
  `map_tuple_key_distinct_alloc`, `map/set/counter_large_under_gc_pressure`,
  `map_overwrite_then_get`, `map_get_absent_returns_unit`,
  `map_index_missing_key_does_not_fault_current_behavior`.
- Deep recursion: `deep_recursion_does_not_crash_host` (safe depth; comment
  reproduces the crash).
- Parser records (the fix): `parser_record_with_text_field_equal_to_literal_
  record`, `..._unequal_when_differs`, `parser_record_text_field_as_map_key`,
  `parser_record_with_text_field_survives_gc`.
- Parser/collection edges: `csv_inside_sections_nonzero_offset`,
  `csv_at_buffer_start_zero_offset`, `read_empty_input_yields_empty_vec`,
  `grid_rotate_four_times_is_identity`, `grid_transpose_twice_is_identity`,
  `grid_equality_false_for_different_content`, `grid_large_under_gc_pressure`,
  `bitset_remove_high_bit_then_equals_untouched`, `bitset_large_under_gc_
  pressure`, `min_heap_ordering_under_gc_pressure`, `tuple_with_record_field_
  equality`, `nested_vec_equality_deep`, `nested_vec_equality_unequal_leaf`,
  `two_faults_in_sequence_clean`, `out_of_bounds_vec_get_faults`,
  `out_of_bounds_vec_negative_index_faults`.

**Totals:** 728 tests pass workspace-wide (up from 620 at M8-WS11 close); the
JIT crate alone grew 205 → 313. `cargo fmt --check` + `cargo clippy -D warnings`
+ `cargo test --workspace` all clean.

**Follow-up fix totals:** the four leftover-fix commits (§2, §3, §6.1-residual,
§6.2, §6.3) added 8 new JIT tests (schema-cache collision ×2, read-against-Unit,
deep-recursion over-limit, closure-from-collection ×4) and converted the two §3
limitation assertions (one now positive, one now documents the residual). The
JIT crate grew 313 → 321; `cargo fmt --check` + `cargo clippy -D warnings` +
`cargo test --workspace` all clean.
