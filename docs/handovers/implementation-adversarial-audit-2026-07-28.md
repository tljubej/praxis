# Implementation adversarial audit — 2026-07-28

## Status

This is a test-and-handover audit, not a repair pass. No production behavior was
intentionally changed.

The audit covered all 15 workspace crates and all 57 Markdown files visible in
the repository (root instructions/readme, 37 ADRs, all milestone handovers, the
previous M8 adversarial audit, the technical design, and the integration-test
readme).

The test suite now has a net 156 additional `#[test]` functions. Confirmed
defects are executable regressions with a specific
`#[ignore = "known bug: ..."]` reason. Passing guards remain active. Existing
tests that did not establish their stated property were strengthened or renamed
where practical.

Validation at handover:

- Baseline before the audit: `cargo test --workspace` passed.
- After the additions: `cargo fmt --all -- --check`, `git diff --check`, and
  `cargo test --workspace` pass.
- Representative ignored tests were run individually and failed at their
  intended assertions:
  - duplicate runtime type ID for `Float` and `Text`;
  - reversed type-variable level lowering;
  - same-line statements accepted without a separator;
  - `enumerate` materializing zero-field tuples.
- Two unsafe-boundary regressions are `#[cfg(miri)]` and are absent from the
  normal suite.

Do **not** run every ignored test in one process. Some deliberately exercise a
known invalid descriptor/payload pairing, a dangling `GcRef`, a null value
through a non-null ABI, or an empty-separator infinite loop. Run an individual
regression after reading its comment. The validation section below lists safe
examples.

## Executive summary

The green pre-audit suite materially overstated implementation safety. The most
urgent findings are representation and rooting defects, not language-design
preferences:

1. `Float` and `Text` both use runtime `TypeId(5)`. Safe-looking typed accessors,
   equality, hashing, parser input checks, and debugger recovery can consequently
   dispatch `Text` operations over an `f64` payload or vice versa.
2. MIR uses the real arena handle `Type(0)` as an “unknown” sentinel. Its meaning
   changes with allocation order and can select an unrelated descriptor.
3. Closure capture indices are raw integers moved through GC-rootable locals.
   Capture index `1` can be spilled and traced as pointer `0x1`.
4. Generated fault and stack-overflow paths return integer zero as a `GcRef`,
   although `GcRef` is backed by `NonNull`.
5. `SourceMap::get` returns a reference after releasing the lock and extending
   its lifetime, while later `intern` calls can move the backing `Vec`.
6. Automatic GC roots omit the ambient input, parse-failure partial value, crash
   snapshot, and native helper intermediates.
7. Codegen hides boxed allocations behind MIR operations declared non-safepoints,
   so liveness does not spill values those allocations can collect.
8. Heap allocation and payload access disagree for over-aligned payloads.
9. Descriptor selection and scalar comparison have unsafe generic `Int`
   fallbacks for payloads that are not `i64`.
10. The input-parser interpreter mixes absolute cursors with relative consumed
    lengths, fails to bound child parsers, and can manufacture collections whose
    descriptor disagrees with every contained object.

These are release blockers because they allow undefined behavior, dangling
references, host panics across `extern "C"`, hangs, or silent type confusion.

## Severity

- **P0**: memory safety, ABI undefined behavior, dangling/non-null invariant
  violation, or cross-layout type confusion.
- **P1**: wrong result, compiler panic, runtime hang, severe type soundness hole,
  or unbounded memory in ordinary repeated use.
- **P2**: observable inconsistency, rejected valid program, missing diagnostic,
  nondeterminism, or misleading coverage.
- **P3**: contract/documentation drift that is not currently on an executed
  unsafe path.

## P0 release blockers

### P0-01 — `Float` and `Text` share runtime identity

`praxis_runtime::scalars::FLOAT` and `praxis_runtime::text::TEXT` are both
`TypeId(5)`. Type IDs are used as runtime identity by typed accessors,
`GcRef::equals`, parser input validation, debugger type recovery, and collection
dispatch.

Consequences include:

- `Float.as_text()` can pass the ID guard and interpret an `f64` as
  `TextPayload`;
- `Text.as_float()` can read text storage as floating-point bits;
- Float/Text equality can call one descriptor's callback on the other's layout;
- `praxis_run_parser` accepts a Float as its supposedly Text input;
- the debugger cannot distinguish the two types.

Regression:
`descriptor::tests::builtin_type_ids_are_globally_unique`.

Fix direction: define all built-in IDs once in a single exhaustive registry.
Do not permit independent numeric constants. Add compile-time uniqueness checks
where possible.

### P0-02 — `Type(0)` is both a valid type and “opaque”

MIR lowering uses `Type(0)` for call/method results, closure self, fields,
tuple/record/enum allocations, parser values, pipeline items/results/
accumulators, and debug metadata. `Type(0)` is a valid TypeDb arena index, not a
sentinel. The type it denotes depends on which type happened to be allocated
first.

This feeds unrelated static types into `descriptor_for_type`, schema generation,
root metadata, and operation selection.

Regressions are in `praxis-mir/src/build.rs` and
`praxis-codegen-cranelift/tests/adversarial_audit.rs`.

Fix direction: make the state explicit and unforgeable, for example:

```text
MirType = Known(Type) | Opaque
```

Then remove every numeric sentinel and require descriptor-producing paths to
hold `Known(Type)`.

### P0-03 — raw closure capture indices inhabit GC locals

Closure capture indices are ABI words, but MIR moves them through
`LocalKind::Gc`. Liveness may spill capture index `1` into the shadow stack and
the collector may dereference it as address `0x1`.

Regression:
`closure_capture_indices_never_flow_through_gc_locals`.

Fix direction: add an explicit raw-word/index operand or local kind. A scalar
must be structurally unable to enter the GC root set.

### P0-04 — generated fault paths return a null `GcRef`

Cranelift fault and stack-overflow epilogues return integer zero while their
logical return type is `GcRef(NonNull<...>)`. Constructing the Rust value is
undefined behavior. The runtime contract already provides a valid Unit sentinel.

Regression:
`fault_epilogue_returns_the_valid_unit_sentinel` uses a raw integer return
channel so the test itself does not instantiate an invalid `GcRef`.

Fix direction: make every generated exit return `RuntimeContext.unit_ref` after
setting the fault.

### P0-05 — `SourceMap::get` can return a moved object

`SourceMap::get` releases its `RwLock` guard and unsafely extends a reference
into `Vec<SourceFile>`. A later `intern` can reallocate the vector, invalidating
the live `FileView`; cross-thread interning has the same problem.

Miri-only regression:
`file::tests::regression_file_view_remains_valid_when_more_files_are_interned`.

Fix direction: use stable ownership (`Arc<SourceFile>` or boxed entries) or keep
the read guard alive for the entire view lifetime.

### P0-06 — automatic GC omits runtime-owned roots

`abi::maybe_collect` walks only `RuntimeContext.roots` and returns early when
that pointer is null. It omits:

- `RuntimeContext.input_source`;
- `Runtime.parse_detail.fail.partial`;
- the runtime-owned crash snapshot.

All are documented owners of live `GcRef`s. A later wrapper allocation can
sweep them.

Regressions:

- `automatic_gc_roots_the_ambient_input_buffer`;
- `automatic_gc_roots_parse_failure_partial_values`;
- `automatic_gc_roots_runtime_owned_crash_snapshots`.

The active
`explicit_collection_preserves_values_held_by_a_crash_snapshot` test proves
only that the snapshot's explicit `RootSet` implementation works.

Fix direction: construct one composite runtime root set for every collection.
It must contain shadow frames, input, parse detail, crash snapshot, and active
native scopes.

### P0-07 — native helpers lose intermediates at nested safepoints

Grid `positions`, `neighbors`, and `find_all` keep their result Vec only in a
Rust local, then call `alloc_point`. `alloc_point` reaches safepointed tuple
allocation. Under pressure, the result Vec can be finalized before the helper
continues mutating its stale payload pointer.

Regression:
`nested_allocating_helpers_root_intermediate_results`.

The parser interpreter has the same architectural problem in reverse: it holds
intermediate refs in Rust Vecs and allocates directly from the heap, so it
currently avoids automatic collection at the cost of unbounded growth. Adding
safepoints without native roots would expose use-after-free.

Fix direction: introduce RAII native root scopes and require every allocation
capable helper/interpreter to register intermediates.

### P0-08 — MIR/codegen disagree about integer-allocation safepoints

MIR models `IntBinOp` as a scalar non-safepoint. Codegen boxes operands/results
through allocating runtime calls. Liveness therefore does not spill values that
these hidden allocations may collect. On a fault, codegen also attempts
`praxis_int_load` on the Unit sentinel before checking the pending fault.

The runtime additionally omits `maybe_collect` in many boxed-return wrappers
(checked integer arithmetic, several conversions/predicates, text allocation,
and dimension/length helpers). Stdlib `allocates` metadata disagrees with actual
allocation behavior.

Regressions include:
`checked_int_add_is_an_automatic_gc_safepoint` and the MIR safepoint/root tests.

Fix direction: define allocation effects once and consume them in stdlib
catalog, MIR, codegen, and runtime wrappers. Add a verifier rejecting a
GC-producing instruction whose live GC operands are not rooted.

### P0-09 — over-aligned payloads are initialized and read at different addresses

`Heap::alloc_raw` rounds the payload start up to its requested alignment.
`GcHeader::payload` always advances by exactly `size_of::<GcHeader>()`. For an
alignment larger than the header's, tracing, access, and finalization use the
wrong address.

Regression:
`overaligned_payload_accessor_matches_initialized_address`.

Fix direction: store the payload offset in the header or make the allocation
layout a single authoritative calculation used by both allocation and access.

### P0-10 — the safe root interface accepts foreign or stale objects

A `RootSet` can yield a `GcRef` from another heap. Collecting heap B then marks
the heap-A object black, delaying its collection by heap A. A stale ref can make
marking dereference finalized memory. `GcRef` carries no heap provenance or
lifetime.

Regression:
`foreign_heap_root_cannot_delay_reclamation`.

Fix direction: associate every allocation with a heap identity and reject roots
not present in that heap's live registry before reading their headers.

### P0-11 — descriptor fallbacks dispatch through the wrong payload layout

`descriptor_for_type` defaults to `INT` for unsupported types. Affected shapes
include Float, Unit, Record, Enum, Func/closure, Range/Seq, and reserved Byte.
The fallback feeds collection element descriptors, tuple/record schemas, and
debug metadata.

Related runtime defects:

- Grid `positions`/`neighbors`/`find_all` return Tuple objects in Vecs tagged
  `Int`;
- Grid `cells`/`row`/`column` discard the cell descriptor and use `Int`;
- `Grid[T](width,height)` fills T-tagged cells with Unit;
- `chars(int)` advertises `Vec[Char]` while storing Int objects;
- a single anonymous `{word}` template creates Text objects under an Int
  descriptor;
- `praxis_vec_push` can retag an explicitly typed Vec after the first push.

Formatting, equality, hashing, tracing, or element access can then call a
callback for the wrong layout.

Representative regressions:

- `empty_vec_float_has_the_float_element_descriptor_before_any_push`;
- `tuple_schema_uses_the_unit_descriptor_for_unit_elements`;
- `tuple_schema_uses_the_enum_descriptor_for_enum_elements`;
- `grid_position_vectors_use_the_point_tuple_descriptor`;
- `grid_cell_vectors_preserve_the_grid_element_descriptor`;
- `constructed_grid_cells_satisfy_the_declared_element_descriptor`;
- `chars_result_descriptor_matches_the_values_it_contains`;
- `single_anonymous_template_capture_uses_its_child_descriptor`;
- `vec_push_rejects_a_value_with_the_wrong_descriptor`.

Fix direction: descriptor selection must be exhaustive and return `Result` for
types with no runtime representation. Collection constructors must require
their descriptor and mutation must reject mismatched objects.

### P0-12 — generic ordering/equality reinterpret payloads as `i64`

HIR never calls its ordering capability checks. MIR sends every non-Float
ordering operation through integer extraction. Bool/Char payloads are smaller
than eight bytes; Text is a pointer/layout structure. Heap ordering likewise
reads every element as `i64`; Float gets signed bit-pattern ordering rather than
numeric ordering.

`DynamicKey::eq` also calls the left descriptor's equality callback on the
right payload without first requiring descriptor identity. The Float/Text ID
collision makes even the nominal ID guard insufficient.

Regressions cover Bool/function/composite ordering, Float heaps, Text/Char JIT
ordering, and heterogeneous dynamic keys.

Fix direction: add descriptor-level semantic compare callbacks or restrict
ordering to types with a proven lowering. Equality must require compatible
runtime descriptors before callback dispatch.

### P0-13 — ABI declarations do not agree

Generated functions/runtime imports use Cranelift `Fast`, while Rust helpers are
`extern "C"`. Public `RunnableFunction` includes a phantom `GcRef` parameter
although zero-source-argument `main` is compiled as `(ctx)`. Several `u32`
arguments are declared I64, and GC pointers are hardcoded as I64 instead of the
target ISA pointer type.

This happens to work on the current host but is not a portable ABI.

Fix direction: define every ABI signature once, derive target pointer types from
the ISA, and generate both declaration and call wrappers from that definition.

### P0-14 — raw syntax kind conversion can construct an invalid enum

The safe Rowan `Language::kind_from_raw` boundary uses an unchecked enum
transmute after only a debug assertion. Not every `u16` is a valid
`SyntaxKind`, despite the implementation comment.

Miri-only regression:
`language::tests::out_of_range_raw_kind_maps_to_a_safe_error_kind`.

Fix direction: checked conversion with an `ERROR` fallback, or a representation
where every raw value is valid.

## Type inference, resolution, HIR, and monomorphization

### TypeDb representation and generalization

| ID | Sev | Finding | Regression / fix note |
|---|---|---|---|
| TY-01 | P1 | `lower_levels` has its comparison reversed: it raises outer variables instead of lowering deeper reachable variables. Inner variables can be unsoundly generalized. | `linking_an_outer_var_to_an_inner_type_prevents_inner_generalization`. Correcting it also requires moving recursive-function placeholders to the function/group inference level; the current placeholder is minted too early and would otherwise pull all params/results to level 0. |
| TY-02 | P1 | Scheme instantiation clones free, non-quantified unbound variables on every use, breaking their shared monomorphic identity. | `instantiation_preserves_non_quantified_variable_identity`. |
| TY-03 | P1 | `Scheme` can encode an invalid state: a monotype points at vars later globally mutated to `Generalized` while the scheme's quantified list is empty. | Replace mutable global generalized state with an explicit scheme-owned quantified mapping. |
| TY-04 | P1 | `deep_resolve` skips record-field and enum-payload side tables, violating its “no links remain” contract and leaving debugger/metadata types stale. | `deep_resolve_rewrites_record_field_links`. |
| TY-05 | P2 | Enum representation documents `Some([])` as equivalent to no payload, but unification rejects the pair. | `empty_enum_payload_and_no_payload_are_equivalent`. Normalize at construction. |
| TY-06 | P1 | Named enum unification is structural by name/signature although nominal identity is documented elsewhere. The representation cannot distinguish compiler instantiations from distinct declarations. | Carry `EnumDefId` through nominal types and instantiate payload args separately. |
| TY-07 | P1 | Type constructors admit illegal tuple/collection arities and duplicate record fields/enum variants. | Validate in constructors, not only at syntax callers. |

### Annotation and scope loss

| ID | Sev | Finding |
|---|---|---|
| TY-08 | P1 | AST type accessors cast only `TYPE_REF`; direct tuple and function type nodes are silently invisible on lets, vars, params, returns, fields, and enum payloads. |
| TY-09 | P1 | User enum annotations resolve by name but are never converted into inference types; `lookup_enum_type` is dead. |
| TY-10 | P1 | Forward struct annotations resolve but are inferred before their TypeDb definition exists and degrade to fresh variables. |
| TY-11 | P1 | Type annotation validation accepts value/function symbols in type position. |
| TY-12 | P2 | Collection type arity is not consistently validated; `Option` silently ignores extra arguments. |
| TY-13 | P1 | Inferer creates scopes disconnected from resolver scopes. Assignment uniquely does a new lookup, so local/param/pattern assignments may be skipped or bind a same-named root symbol. |
| TY-14 | P1 | Assignment never requires `SymbolKind::Var`; immutable lets and parameters can be reassigned. |
| TY-15 | P1 | Compound assignment checks only same-type operands, allowing operations such as `Bool += Bool`. |

Regressions are named directly after these contracts in
`praxis-hir/src/infer_tests.rs`, including tuple/function annotations, enum
annotations, bad type-position names, collection arity, local reassignment,
immutability, and compound-assignment capability.

### Control-flow type drift

| ID | Sev | Finding |
|---|---|---|
| TY-16 | P1 | Block inference retains the last expression it saw even when a later statement follows; lowering demotes that expression, so inferred and executed result types differ. |
| TY-17 | P1 | `if` without `else` takes the then-branch type while MIR's false path yields Unit. |
| TY-18 | P1 | Explicit `return` values are never unified with the declared/inferred function result. |
| TY-19 | P1 | `Never` is not a bottom type in unification; valid divergent branches are rejected or joined to the wrong type. |
| TY-20 | P2 | There is no function/loop context tracking, so top-level `return` and out-of-loop `break`/`continue` pass analysis. |
| TY-21 | P2 | `loop { break value }` remains Unit despite the documented expression-loop semantics. |
| TY-22 | P1 | Forward calls resolve but are not checked against a later function signature; placeholders are created only when visiting the declaration. |
| TY-23 | P1 | A nested function item reaches `expect` and panics, violating the analyzer's no-panic contract. |
| TY-24 | P1 | Duplicate top-level function declarations survive to duplicate JIT symbols. |

### Operator, prelude, and capability constraints

| ID | Sev | Finding |
|---|---|---|
| TY-25 | P1 | `parse(text, parser)` does not constrain its first argument to Text. |
| TY-26 | P2 | Unary minus accepts Float only when it recognizes literal syntax; a Float-typed variable is rejected. |
| TY-27 | P1 | Float remainder typechecks, while MIR's unsupported fallback maps it to addition. |
| TY-28 | P1 | Out-of-range integer literals silently become `i64::MAX` in HIR. |
| TY-29 | P1 | Equality, Iterable, Numeric, Hash, and Ord constraints are discarded at generalization; incompatible later instantiations pass. |
| TY-30 | P2 | Method lookup on an unresolved receiver cannot constrain generic parameters, contradicting the documented `values.sum()` inference example. |
| TY-31 | P1 | Numeric sinks (`sum`, `product`, `min`, `max`) do not enforce numeric/orderable element types; `Vec[Bool].sum()` passes. |
| TY-32 | P1 | Map/Set key hashability and Heap element orderability are not enforced. Even the current `supports_hash` helper equates hashability with equality and therefore admits mutable collections. |
| TY-33 | P1 | Most canonical prelude names are just strings: they have neither a real scheme nor MIR/runtime dispatch. `assert`, `dbg`, arithmetic helpers, and graph helpers receive fresh types then lower as missing user functions. |
| TY-34 | P2 | Range is wired in types/MIR but absent from the prelude and runtime construction path. |

### HIR lowering and records/matches

| ID | Sev | Finding |
|---|---|---|
| HIR-01 | P1 | Lowering re-instantiates symbol/callee schemes instead of consuming each inferred use type. Generic call and method results become disconnected from arguments/receivers, causing typed-HIR/MIR descriptor and operation drift. |
| HIR-02 | P2 | Method hover records a type at a method token that is absent from `refs`, so the information is unusable. |
| HIR-03 | P1 | Enum call/path lowering checks constructor text in root scope before the resolved symbol; a local shadowing a variant is lowered as the constructor. |
| HIR-04 | P1 | Record literals accept missing, unknown, and duplicate fields. Missing fields become Unit under declared field types; duplicate payloads misalign schemas; unknown initializers are not lowered, so side effects disappear. |
| HIR-05 | P1 | `_` lexes as `Ident`, so wildcard patterns are ordinary catch-all bindings and `_` is readable in the arm body. |
| HIR-06 | P1 | Exhaustiveness checks only top-level variants/catch-all position. Nested enum payload gaps and duplicate constructor arms are missed. |
| HIR-07 | P1 | An unknown constructor pattern lowers to wildcard, potentially making a typo exhaustive. Its diagnostic span is also always zero. |
| HIR-08 | P1 | Escape analysis skips the callee expression of an immediately invoked closure; a mutable capture is not boxed. |
| HIR-09 | P2 | Capture analysis still carries obsolete “mutable capture unsupported” errors, while mutable captures are now implemented. A capture first seen on an assignment LHS can also fall back to a fresh type. |

### Monomorphization

| ID | Sev | Finding |
|---|---|---|
| MONO-01 | P1 | Monomorphization unifies a fresh scheme instance but follows types from an unrelated original `TypedFn`. Clones are renamed but their parameter/body/result types are not substituted. |
| MONO-02 | P1 | Zero-argument generic calls are skipped and have no result-context witness. |
| MONO-03 | P1 | Cache keys use rendered types; enum rendering omits payload arguments, so e.g. `Option[Int]` and `Option[Text]` can share a clone. |

Regressions:
`specialized_clone_carries_concrete_types_throughout`,
`zero_argument_generic_result_is_specialized_from_use_context`, and
`enum_payload_types_participate_in_monomorphization_cache_key`.

## MIR, liveness, JIT, and pipelines

| ID | Sev | Finding |
|---|---|---|
| MIR-01 | P0 | Shadow/debug spill slots are never cleared when a value ceases to be live. Stale roots retain objects and can expose a raw non-GC word to collection. |
| MIR-02 | P1 | Liveness's second pass walks forward and only adds definitions, so roots grow monotonically within a block instead of dying after last use. |
| MIR-03 | P1 | `take(n)` and `skip(n)` recognize only literal arguments. An arbitrary Int typechecks but falls through to a Unit intrinsic stub. |
| MIR-04 | P1 | Filtered pipelines use sparse source indices for `take`, `skip`, `zip`, `find`, and `position`. |
| MIR-05 | P1 | `enumerate` and `zip` allocate tuples with `Type(0)`, producing zero-field schemas and dropping their claimed values. |
| MIR-06 | P1 | A second `flat_map` reaches `unreachable!` and panics the compiler. |
| MIR-07 | P1 | Indices after `flat_map` reset for every inner Vec instead of tracking the global flattened stream. |
| MIR-08 | P1 | `any`, `find`, and `take_while` short-circuit only the inner flat-map loop. |
| MIR-09 | P0 | Empty `reduce`, `min_by`, and `max_by` return an uninitialized GC accumulator with no defined fault/Option. |
| MIR-10 | P1 | Pipeline items and accumulators cross safepoints without a verifier enforcing the ADR-015 root invariant. |
| MIR-11 | P1 | `continue` in a `for` loop jumps to the header, skipping the increment block and potentially looping forever. |
| MIR-12 | P1 | Process-global record schemas are keyed only by `RecordDefId(u32)`, which restarts in each TypeDb. A later JIT generation can reuse an incompatible schema. |
| MIR-13 | P1 | JIT metadata (text, names, debug records, tuple schemas, record schemas) is leaked through `Box::leak`; there is no reclaimable generation arena. |
| MIR-14 | P1 | Runtime symbol registration is incomplete and duplicated. Several names resolved by `symbols.rs` are omitted from `module.rs`; tests rely on platform fallback lookup. |
| MIR-15 | P2 | Float bitcasts, pointer types, and ABI widths assume little-endian 64-bit instead of the target ISA. |
| MIR-16 | P1 | Debug metadata inherits `Type(0)` descriptors and stale spill state. Some debugger tests pass because over-liveness accidentally preserves operands. |

The main end-to-end regression file is
`crates/praxis-codegen-cranelift/tests/adversarial_audit.rs`. It deliberately
inspects result payloads and descriptors, rather than merely counting outer
collection elements.

## Runtime, GC, memory model, and collections

### Collection and lifetime defects

| ID | Sev | Finding |
|---|---|---|
| RT-01 | P1 | Sweep finalizes/removes dead objects but bumpalo cannot reuse individual blocks. Repeated bounded allocate/collect cycles grow arena memory forever while `live_count` returns to zero. |
| RT-02 | P1 | `Heap` has no `Drop` that finalizes still-live Text/Vec/Map/etc. Arena bytes go away, but nested Box/Vec/HashMap allocations leak. |
| RT-03 | P1 | Bool/Unit ABI helpers allocate a new immortal on many calls instead of returning the runtime's cached three singleton objects. Every call consumes unregistered arena storage permanently. |
| RT-04 | P1 | GC pacing counts only fixed GC object layout, not Text/Vec/HashMap backing allocations. Explicit collection doubles the threshold each time; `reset` leaves pacing fields unchanged. |
| RT-05 | P0 | Safe `Runtime::heap_mut().reset()` invalidates the runtime's cached immortal refs and leaves context Unit/Bool pointers dangling. |
| RT-06 | P0 | Safe `alloc_text_slice` only debug-checks range and UTF-8 boundaries. Release invalid ranges can panic or silently return empty/truncated text. |
| RT-07 | P1 | Negative/user-controlled sizes can cast or resize toward huge `usize` values (`Grid`, `BitSet`), and neighbor arithmetic can overflow. These host panics/OOMs cross `extern "C"`, violating the no-panic ABI. |

Regressions cover arena reuse, live-payload finalization, singleton reuse, and
GC pacing. The reset/text-range/huge-size cases should receive safe
representation-level tests before executing hostile values through FFI.

### Equality, hashing, ordering, and absence

| ID | Sev | Finding |
|---|---|---|
| RT-08 | P1 | Mutable structurally hashed values can be Map/Set keys. Mutation changes the hash bucket key in place, making entries unreachable or duplicable and violating Rust hash-table invariants. |
| RT-09 | P0 | `DynamicKey::eq` omits descriptor identity, can call a callback on the wrong layout, and can disagree with Hash. |
| RT-10 | P2 | Empty collections of different runtime type arguments can compare equal because per-instance descriptors are omitted. |
| RT-11 | P2 | Tuple equality requires schema pointer identity. Parser, runtime point, and codegen caches can create distinct but identical shapes that compare unequal. |
| RT-12 | P1 | Record equality also relies on schema identity; process/generation cache collisions can make it either too strict or dispatch through a stale shape. Nominal and anonymous identity need separate explicit representations. |
| RT-13 | P1 | Runtime enums carry tag/payload but no nominal enum definition identity. Distinct enum definitions with the same shape can compare/hash equal if they reach generic dispatch. |
| RT-14 | P1 | `Map.get` is statically `V` and non-faulting but returns Unit when absent. |
| RT-15 | P1 | `Grid.find` is statically Tuple and non-faulting but returns Unit when absent. |
| RT-16 | P2 | Map/Set/Counter formatting follows randomized hash-table iteration; BinaryHeap iteration is not sorted. Output is nondeterministic despite the formatting contract. |

Absence must be represented as `Option[V]`/`Option[Point]` or a checked fault,
not an untyped Unit smuggled under another static type.

### Fault and scalar inconsistencies

| ID | Sev | Finding |
|---|---|---|
| RT-17 | P1 | `Fault::set(FaultKind::None)` creates `{pending:true, kind:None}`. Invalid Char and malformed UTF-8 paths use it, so generated code branches on a fault the host reports as “none”. |
| RT-18 | P1 | Char conversion casts `i64` to `u32` before validation. Values such as `0x1_0000_0041` silently become `'A'`. |
| RT-19 | P2 | `Float.sign(±0)` delegates to `signum` and returns ±1 rather than the documented zero. |

## Input parser: compiler/scanner/validator

| ID | Sev | Finding |
|---|---|---|
| IP-01 | P1 | Template scanning iterates UTF-8 bytes and converts each byte with `char::from`, corrupting non-ASCII literal text. |
| IP-02 | P2 | A terminal backslash is accepted as ordinary text instead of an invalid escape. |
| IP-03 | P2 | Invalid `\s` diagnostics duplicate the `s` (`\\ss`). |
| IP-04 | P2 | Capture-name recognition is ASCII-only despite Unicode identifiers elsewhere. |
| IP-05 | P1 | The scanner discards each capture parser body. HIR later scans the full template from the beginning for every capture, so every capture gets the first recognizable kind. `{name:word},{port:int}` becomes word + word. |
| IP-06 | P1 | An unknown capture kind silently defaults to Int. |
| IP-07 | P1 | Constructor lowering discards invalid/extra args before validation (`optional`, `scan`, `matrix`, `one_of`, `chars`, and named builders). Unknown constructors can become `None` with no diagnostic. |
| IP-08 | P2 | Separator/choice string literals strip quotes but do not unescape contents. |
| IP-09 | P1 | Named sections do not check repeated-tail fields against fixed field names; multiple tails/order can be silently normalized or overwritten. |
| IP-10 | P1 | Empty `sep("", P)` passes validation and reaches a runtime loop whose cursor never advances. |
| IP-11 | P2 | Required technical-design atomics (`uint`, `float`, `byte`, `identifier`) are not implemented. |
| IP-12 | P1 | Parser plans, literals, schemas, and string storage are leaked for process lifetime; plan indices also narrow to `u32` without a checked bound. |
| IP-13 | P3 | `PlanNode` documentation describes a flat `repr(C)` plan, but the Rust enums/slices do not have that representation. It is not presently passed over FFI, so this is contract drift rather than an active ABI bug. |

## Input parser: runtime interpreter

| ID | Sev | Finding |
|---|---|---|
| IPR-01 | P1 | `WalkResult.consumed` is documented/used as an absolute cursor, but many combinators return `bytes.len() - offset` (a relative length). Nested non-zero-offset parsing can move backward or skip data. |
| IPR-02 | P1 | `lines` does not bound the child to one line or require full child consumption. `lines(int)` accepts `12junk`; `lines(rest)` captures through EOF. |
| IPR-03 | P1 | `sections` parses a subslice at offset zero while Text slices keep the original input owner, so later-section Text points at the beginning. Matrix/tokenized bounded paths share the ownership/offset problem. |
| IPR-04 | P1 | `csv`, `ws`, and `sep` find token bounds but call the child on the full remaining input and often ignore its consumed value. Duplicate CSV fields can map to the first occurrence. |
| IPR-05 | P1 | Matrix parsing uses token-local offsets as if absolute, does not require whole-token consumption, and converts invalid UTF-8 to empty text. |
| IPR-06 | P1 | Grid/ragged-grid loops and widths count UTF-8 bytes, not Unicode scalars. Child parsers run on whole suffixes; fill Text offsets are also wrong. |
| IPR-07 | P1 | `chars` stops successfully at the first child failure and silently drops the tail. Its `chars(int)` descriptor/value mismatch is a P0 layout issue above. |
| IPR-08 | P1 | `scan` advances one byte through UTF-8 continuation bytes. |
| IPR-09 | P2 | `choice` loses the deepest useful failure and accepts successful prefixes without consistently requiring region exhaustion. |
| IPR-10 | P1 | Atomic Text always consumes the rest of the region, so a capture such as `pre{body:text}post` consumes its suffix literal. |
| IPR-11 | P2 | `word` stops only on a small delimiter set rather than all template delimiters. |
| IPR-12 | P1 | Whitespace representation is internally contradictory: scanner literals retain spaces while also tagging them as whitespace, so runtime may consume then match them twice. `SpaceRun` is documented one-or-more but accepts zero. |
| IPR-13 | P1 | Single anonymous template and nested constructor descriptors are frequently hardcoded Int instead of derived from child plans. |
| IPR-14 | P1 | `choice` branch allocations remain live until a later collection; with current no-safepoint parsing this grows memory, while adding a safepoint without native roots would dangle intermediates. |

## Frontend, parser, formatter, source, CLI, and debugger

### Lexer/parser/formatter

| ID | Sev | Finding |
|---|---|---|
| FE-01 | P1 | Identifier-start classification is ASCII-byte-only. A leading Unicode scalar is split into unknown bytes despite the UTF-8/Unicode identifier contract. |
| FE-02 | P1 | `_` is emitted as Ident, never `UNDERSCORE`; wildcard semantics are accidentally implemented as a normal binding. |
| FE-03 | P2 | `SyntaxKind::is_keyword` omits `KW_IN`. The previous “every keyword” test copied the same incomplete list and therefore passed. |
| FE-04 | P1 | Statement separation discards trivia without inspecting whether it contained a newline. Same-line statements need no semicolon, top-level semicolons are not consumed, and `return\n1` becomes `return 1`. |
| FE-05 | P1 | Postfix parsing runs a call loop followed by a field/method loop but never returns to calls. General interleavings such as `(fs).get(0)(100)` fail. |
| FE-06 | P1 | Global `no_struct_literal` suppression leaks through parentheses and all match-arm bodies, rejecting valid nested/parenthesized record literals. |
| FE-07 | P2 | Formatter rebuilds from descendant tokens while omitting comment tokens. Idempotence-only tests did not notice comment loss. |
| FE-08 | P2 | Fuzz tests claim arbitrary bytes but generate Rust `String`, which is valid UTF-8 only. Invalid-byte lexer boundaries are untested. |

### CLI

CLI input reads use `unwrap_or_default` for explicit files/stdin. A missing or
unreadable `--input` is silently treated as empty input instead of the documented
usage/I/O exit code 2.

Regression:
`missing_explicit_input_file_is_a_usage_error`.

### Debugger

| ID | Sev | Finding |
|---|---|---|
| DBG-01 | P0 | Scalar TypeId recovery is stale: runtime Unit=0, Bool=1, Int=2, Byte=3, Char=4, Float=5, Text=5, while debugger maps 0/1/4 to Int, 2 to Bool, 3 to Char, and 5 to Text. |
| DBG-02 | P1 | Collection type recovery hardcodes all type arguments as Int; a runtime `Vec[Text]` is reconstructed as `Vec[Int]`. |
| DBG-03 | P2 | Name sanitization rejects Unicode and maps every invalid name to `_x`, creating collisions. |
| DBG-04 | P0 | Evaluator creates a `RootScope` for snapshot/arguments but never installs it in `RuntimeContext.roots`. Generated prologues can mask argument loss, while omitted snapshot locals remain collectible during REPL evaluation. |
| DBG-05 | P1 | Parser plans/schemas/string/debug metadata are process-global leaks; repeated reload/evaluation grows them without bound. |
| DBG-06 | P1 | A debugger JIT can create schemas independent from the main JIT while runtime tuple/record equality assumes compatible pointer identity/canonicalization. |

Regressions:
`runtime_vec_text_type_is_recovered_as_vec_text` and
`runtime_scalar_descriptor_ids_recover_their_actual_types`.

## Existing tests that did not test what they claimed

1. The keyword “round trip/every keyword” list omitted `in`, mirroring the
   implementation bug.
2. Input type-synthesis tests inspected only outer constructors. They now verify
   `Vec[Int]`, `Grid[Char]`, and every nested Vec layer plus the Int leaf.
3. Existing monomorphization tests inspected clone names/counts only. One test
   named “two instantiations” invoked Int twice and proved reuse, not distinct
   specialization.
4. Existing `enumerate`/`zip` tests counted outer results but never read tuple
   arity or elements, so zero-field tuples passed.
5. `grid_find_locates_first_match` calls `find_all(...).len()` and never invokes
   `Grid.find`.
6. Mixed template-capture JIT tests compare length/equality while every capture
   is assigned the first capture kind. They never use `port` as Int.
7. `adv_parser_record_with_text_field_unequal_when_differs` uses the same input
   on both sides and expects equality; its name and assertion disagree.
8. Line parser tests never require whole-line child consumption or bound `rest`
   to one line.
9. Formatter comment/nesting tests asserted only idempotence; deleting comments
   is itself idempotent.
10. Parser fuzz “arbitrary bytes” tests generate valid `String`.
11. The runtime parser smoke test only asserted that the module existed; it was
    removed.
12. The old source-slice test manually sliced an owned byte buffer and never
    allocated/traced `TextPayload::Slice`. It was renamed; a real active tracing
    test was added.
13. `vec_push_many_survive_collection` supplied no shadow frame, causing
    `maybe_collect` to return early. It now installs roots, proves the live
    registry shrank, and validates elements after collection.
14. Several JIT “GC pressure” tests asserted only final arithmetic results and
    did not prove collection occurred. The new active heavy-loop guard checks
    heap live-count reduction.
15. Type test `scalar_interning_is_stable` never compared handles and claimed
    nonexistent structural interning. It now states and checks distinct handles
    plus structural unification.
16. `nominal_records_same_name_unify` actually asserted rejection for distinct
    DefIds. It was renamed to the nominal invariant it tests.
17. Existing wildcard tests never inspect the typed pattern or whether `_`
    became visible as a binding.
18. `record_with_function_field_not_equatable` passes because its initializer
    pins the fresh type after the annotation was dropped; it did not prove the
    field annotation survived.
19. `unresolved_var_is_optimistically_iterable` tests only the immediate lookup,
    not a later incompatible generic instantiation.
20. `numeric_scalars_are_orderable` includes Text and Char and tests only a
    capability flag, not their unsafe runtime lowering.
21. Exhaustiveness tests cover top-level variants/catch-all only, not nested
    payloads or duplicate constructors.
22. Runtime `map_get_missing_returns_zero` encodes one of three incompatible
    contracts: technical design says `Option[V]`, catalog says `V` with Unit on
    absence, and the old test calls it zero.
23. `prelude_includes_design_canonical_entries` checks only that strings occur in
    a table, not that they have schemes or executable lowering.
24. The old liveness “empty function” test constructed a block but never
    inserted it into the function.
25. The old JIT compile helper ignored parser and analysis diagnostics. The new
    adversarial helper checks parsing, analysis, and lowering before execution,
    and calls zero-argument `main` with its actual `(ctx)` ABI.

## Test inventory

Primary additions:

- `crates/praxis-codegen-cranelift/tests/adversarial_audit.rs`
  - 30 ignored end-to-end regressions;
  - 3 active guards.
- `crates/praxis-hir/src/infer_tests.rs`
  - 54 ignored inference/HIR regressions;
  - 1 active recursive-call signature guard.
- `crates/praxis-hir/src/mono.rs`
  - 3 ignored specialization regressions.
- `crates/praxis-types/src/types_tests.rs`
  - 4 ignored TypeDb regressions;
  - two misleading tests strengthened/renamed.
- `crates/praxis-mir/src/build.rs`
  - 8 ignored lowering/type/root regressions.
- `crates/praxis-mir/src/liveness.rs`
  - 2 ignored shrinking-root regressions.
- `crates/praxis-runtime/src/*`
  - active finalization, source-slice tracing, crash-snapshot rooting, and
    collection-occurrence guards;
  - ignored GC, descriptor, ABI, collection, ordering, and parser regressions.
- `crates/praxis-parser/src/{lex,parse,fmt}.rs`
  - Unicode, wildcard, separator, postfix, record-literal, and comment tests.
- `crates/praxis-input-parser/src/{scan,synthesize,validate}.rs`
  - Unicode/escape, exact nested type shape, empty separator, and repeated-tail
    tests.
- `crates/praxis-{syntax,source,debugger,cli}`
  - raw-kind/keyword, stable SourceMap view, debugger type recovery, and input
    I/O tests.

Every confirmed failing test has a reason on its `#[ignore]` attribute. Tests
which need Miri are gated with `#[cfg(miri)]`, not ordinarily ignored.

## Recommended repair order

The order matters because later fixes otherwise expose latent unsafe paths.

1. **Make runtime identity and values valid by construction**
   - assign unique built-in TypeIds;
   - return Unit rather than null from every generated fault path;
   - replace `Type(0)` with `Known | Opaque`;
   - create a distinct raw ABI-word MIR kind;
   - centralize ABI signatures and descriptor selection.
2. **Repair ownership/rooting before adding more safepoints**
   - stabilize `SourceFile` addresses;
   - fix aligned payload layout;
   - add heap provenance;
   - build a composite automatic root set;
   - add native RAII roots for helpers/parser;
   - synchronize allocation-effect metadata.
3. **Remove descriptor/value lies**
   - exhaustive descriptor mapping;
   - typed collection construction and mutation validation;
   - real Option/fault states for missing Map/Grid results;
   - semantic equality/ordering callbacks.
4. **Repair TypeDb generalization and exact per-use types**
   - correct level lowering and recursive placeholder levels together;
   - redesign schemes so free and quantified variables cannot be confused;
   - carry inferred use types into HIR/MIR;
   - then fix monomorphization substitution/cache keys.
5. **Repair parser cursor/region ownership as one change**
   - introduce `AbsoluteCursor` and `ByteRegion { owner, start, end }`;
   - require full bounded child consumption where the DSL promises it;
   - represent `NonEmptySeparator` so the infinite loop is impossible;
   - derive descriptors from typed plan nodes.
6. **Repair control flow, records/matches, and pipelines**
   - use dense stage-local pipeline indices;
   - define empty sink results;
   - verify loop targets;
   - make record field sets exact;
   - use real wildcard tokens and recursive usefulness checking.
7. **Address bounded memory and determinism**
   - use reclaimable heap blocks or generation arenas;
   - finalize live objects on drop;
   - account for external allocation sizes;
   - make formatting order deterministic.

Examples of “illegal states unrepresentable” types that fit the repository
instruction:

```text
MirType            = Known(Type) | Opaque
ParserCursor       = AbsoluteCursor(usize)
Separator          = NonEmptySeparator(Box<[u8]>)
MapLookup<V>       = Option<V>
GridLookup         = Option<Point>
Collection<T>      = { descriptor: Descriptor<T>, values: Vec<GcRef<T>> }
RuntimeTypeId      = generated exhaustive enum, not independent integers
NativeRootScope    = RAII guard attached to RuntimeContext
SourceFileHandle   = Arc<SourceFile>
HashKey<T>         = only types satisfying immutable HashStable
```

## Validation and safe reproduction

Normal suite:

```sh
cargo fmt --all -- --check
git diff --check
cargo test --workspace
```

Safe representative failing regressions:

```sh
cargo test -p praxis-runtime \
  descriptor::tests::builtin_type_ids_are_globally_unique \
  -- --ignored --exact

cargo test -p praxis-types \
  types_tests::linking_an_outer_var_to_an_inner_type_prevents_inner_generalization \
  -- --ignored --exact

cargo test -p praxis-parser \
  parse::tests::regression_same_line_statements_require_a_semicolon \
  -- --ignored --exact

cargo test -p praxis-codegen-cranelift --test adversarial_audit \
  enumerate_materializes_index_and_element_tuple_payloads \
  -- --ignored --exact
```

Miri-only boundary checks:

```sh
cargo +nightly miri test -p praxis-source \
  file::tests::regression_file_view_remains_valid_when_more_files_are_interned \
  -- --exact

cargo +nightly miri test -p praxis-syntax \
  language::tests::out_of_range_raw_kind_maps_to_a_safe_error_kind \
  -- --exact
```

Do not use `cargo test --workspace -- --ignored` until the P0 descriptor,
rooting, null-`GcRef`, and empty-separator findings are repaired.
