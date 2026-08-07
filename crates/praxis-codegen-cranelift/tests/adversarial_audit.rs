//! Focused MIR/Cranelift regressions found by the post-M10 adversarial audit.
//!
//! These tests intentionally state the language/runtime contract. Some expose
//! known implementation defects and therefore fail until the corresponding
//! handover item is fixed; they must not be weakened to assert current behavior.

use std::fmt::Write as _;

use praxis_ast::AstNode;
use praxis_codegen_cranelift::Jit;
use praxis_hir::{analyze_root, lower, mono::monomorphize};
use praxis_mir::{annotate, lower_module};
use praxis_parser::parse;
use praxis_runtime::{
    collections::VecPayload, tuples::TuplePayload, GcRef, Runtime, RuntimeContext,
};
use praxis_source::SourceMap;

fn compile(
    src: &str,
) -> (
    Jit,
    std::collections::HashMap<String, cranelift_module::FuncId>,
) {
    let map = SourceMap::new();
    let file = map.intern("adversarial_audit.px", src);
    let parsed = parse(file, src);
    assert!(
        parsed.diagnostics.is_empty(),
        "parse diagnostics: {:?}",
        parsed.diagnostics
    );
    let mut analysis = analyze_root(file, &parsed.tree);
    assert!(
        analysis.diagnostics.is_empty(),
        "analysis diagnostics: {:?}",
        analysis.diagnostics
    );
    let root = praxis_ast::SourceFile::cast(parsed.tree.clone()).expect("source root");
    let module = lower(file, &root, &mut analysis);
    assert!(
        module.diagnostics.is_empty(),
        "lowering diagnostics: {:?}",
        module.diagnostics
    );
    let module = monomorphize(module, &analysis.names, &mut analysis.db);
    let mut funcs = lower_module(&module, &mut analysis.db);
    for func in &mut funcs {
        annotate(func);
        // Every host verifies after annotating; the tests are a host too, and
        // are the only one that runs the whole corpus (MIR-10).
        if let Err(errs) = praxis_mir::verify(func) {
            panic!("{}", praxis_mir::verify::report(&errs));
        }
    }
    let mut jit = Jit::new().expect("JIT construction");
    let ids = jit
        .compile(&funcs, &mut analysis.db)
        .expect("JIT compilation");
    (jit, ids)
}

fn run_main(src: &str) -> (Runtime, GcRef) {
    run_main_with_input(src, "")
}

fn run_main_with_input(src: &str, input: &str) -> (Runtime, GcRef) {
    let (jit, ids) = compile(src);
    let main_id = *ids.get("main").expect("main function");
    let mut runtime = Runtime::new();
    let mut context = runtime.context();
    context.input_source = runtime.alloc_text(input);
    // `main` has no source parameters: its generated ABI is exactly `(ctx)`.
    // Do not repeat the legacy test helper's phantom GcRef argument.
    type ZeroArgMain = unsafe extern "C" fn(*mut RuntimeContext) -> GcRef;
    let entry: ZeroArgMain = unsafe { std::mem::transmute(jit.entry(main_id)) };
    let result = unsafe { entry(&mut context as *mut RuntimeContext) };
    drop(jit);
    (runtime, result)
}

/// Call a JIT entry through its raw i64 return channel. Fault epilogues are
/// inspected this way so a buggy zero return is not first materialized as the
/// invalid Rust value `GcRef(NonNull::new_unchecked(0))`.
fn run_main_raw_with_input(src: &str, input: &str) -> (Runtime, usize, usize) {
    type RawZeroArgMain = unsafe extern "C" fn(*mut RuntimeContext) -> usize;

    let (jit, ids) = compile(src);
    let main_id = *ids.get("main").expect("main function");
    let mut runtime = Runtime::new();
    let mut context = runtime.context();
    context.input_source = runtime.alloc_text(input);
    let unit = runtime.alloc_unit();
    let unit_addr = unit.as_ptr() as usize;
    let entry: RawZeroArgMain = unsafe { std::mem::transmute(jit.entry(main_id)) };
    let result = unsafe { entry(&mut context as *mut RuntimeContext) };
    drop(jit);
    (runtime, result, unit_addr)
}

fn tuple_items(value: GcRef) -> Vec<GcRef> {
    assert_eq!(
        value.descriptor().id(),
        praxis_runtime::tuples::TUPLE.id(),
        "pipeline element should be a Tuple"
    );
    let payload = value.payload::<TuplePayload>();
    // SAFETY: the descriptor check above establishes the payload shape. The
    // copied GcRefs remain valid while the Runtime owned by the test is live.
    unsafe { (*payload).items.clone() }
}

/// Each schema slot's descriptor id, or `None` for a **null** slot — "the
/// compiler had no static type here", which is a legal answer (ADR-066
/// decision 5) and the one thing that distinguishes a real element type from
/// the runtime falling back to the value's own header.
fn tuple_element_descriptor_slots(value: GcRef) -> Vec<Option<praxis_runtime::descriptor::TypeId>> {
    assert_eq!(
        value.descriptor().id(),
        praxis_runtime::tuples::TUPLE.id(),
        "value should be a Tuple"
    );
    let payload = value.payload::<TuplePayload>();
    // SAFETY: the descriptor check establishes TuplePayload, and tuple
    // construction installs a process-static TupleSchema.
    let schema = unsafe { &*(*payload).schema };
    schema
        .descriptors
        .iter()
        .map(|descriptor| {
            if descriptor.is_null() {
                None
            } else {
                // SAFETY: a non-null schema entry is a process-static descriptor.
                Some(unsafe { &**descriptor }.id())
            }
        })
        .collect()
}

fn tuple_element_descriptor_ids(value: GcRef) -> Vec<praxis_runtime::descriptor::TypeId> {
    tuple_element_descriptor_slots(value)
        .into_iter()
        .map(|slot| slot.expect("every slot of this tuple should carry a static type"))
        .collect()
}

#[test]
fn fault_epilogue_returns_the_valid_unit_sentinel() {
    // The no-panic fault protocol promises a defined dummy value. Because
    // GcRef is NonNull, integer zero is not a valid dummy; the generated fault
    // epilogue must return RuntimeContext.unit_ref.
    let (runtime, raw_result, unit_addr) =
        run_main_raw_with_input("fn main() -> Int { 1 / 0 }", "");
    assert_eq!(runtime.fault(), praxis_runtime::FaultKind::DivByZero);
    assert_eq!(
        raw_result, unit_addr,
        "fault paths must return the Unit GcRef, never a null i64"
    );
}

#[test]
fn enumerate_materializes_index_and_element_tuple_payloads() {
    // The older enumerate test only counted results. Inspect the claimed
    // `(index, element)` values themselves so an empty TupleSchema cannot pass.
    let (runtime, result) =
        run_main("fn main() {\n  var v = Vec()\n  v.push(10)\n  v.push(20)\n  v.enumerate()\n}\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    let values = result.as_vec();
    assert_eq!(values.len(), 2);
    let first = tuple_items(values[0]);
    let second = tuple_items(values[1]);
    assert_eq!(first.len(), 2, "enumerate tuples have arity two");
    assert_eq!(second.len(), 2, "enumerate tuples have arity two");
    assert_eq!((first[0].as_int(), first[1].as_int()), (0, 10));
    assert_eq!((second[0].as_int(), second[1].as_int()), (1, 20));
}

/// **MIR-05.** The pair a fused `enumerate`/`zip` builds carries the type the
/// method catalog already declares, so its schema slots name real descriptors.
///
/// This is the only assertion that separates MIR-05 from REP-23's fallback.
/// REP-23 made a typeless pair keep its *arity*, so the values survive and
/// `enumerate_materializes_index_and_element_tuple_payloads` passes either way;
/// what it cannot see is that every slot said "no static type" and formatting,
/// hashing and equality were dispatching through the values' own headers rather
/// than through the compiler's answer.
#[test]
fn a_fused_pairs_schema_names_its_element_types() {
    let int = praxis_runtime::scalars::INT.id();
    let text = praxis_runtime::text::TEXT.id();

    // `enumerate` on a Vec[Int] is Vec[(Int, Int)].
    let (runtime, result) =
        run_main("fn main() {\n  var v = Vec()\n  v.push(10)\n  v.enumerate()\n}\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(
        tuple_element_descriptor_slots(result.as_vec()[0]),
        vec![Some(int), Some(int)],
        "enumerate's pair is (Int, T), and both halves are named"
    );

    // `zip` pairs two *different* element types, so a schema that echoed the
    // receiver's element type for both slots would fail here and not above.
    let (runtime, result) = run_main(
        "fn main() {\n  var a = Vec()\n  a.push(1)\n  var b = Vec()\n  b.push(\"s\")\n  a.zip(b)\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(
        tuple_element_descriptor_slots(result.as_vec()[0]),
        vec![Some(int), Some(text)],
        "zip's pair is (T, U), and the two halves differ"
    );

    // The mirror case, which must stay reachable: a receiver whose element type
    // is still an inference variable compiles, and its unresolved half becomes a
    // null slot rather than a compile error (ADR-066 decision 5). This is why
    // the verifier's `OpaqueAtDescriptorSite` rule stays off — turning it on
    // would refuse a program that works.
    let (runtime, result) =
        run_main("fn main() -> Int {\n  var v = Vec()\n  v.enumerate().count()\n}\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(
        result.as_int(),
        0,
        "an unpushed Vec still enumerates to empty"
    );
}

#[test]
fn zip_materializes_both_tuple_elements() {
    // Counting zipped values does not prove that either tuple element was
    // stored. Read both payload slots.
    let (runtime, result) = run_main(
        "fn main() {\n  var a = Vec()\n  a.push(1)\n  a.push(2)\n  var b = Vec()\n  b.push(10)\n  b.push(20)\n  a.zip(b)\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    let values = result.as_vec();
    assert_eq!(values.len(), 2);
    let first = tuple_items(values[0]);
    let second = tuple_items(values[1]);
    assert_eq!(first.len(), 2, "zip tuples have arity two");
    assert_eq!(second.len(), 2, "zip tuples have arity two");
    assert_eq!((first[0].as_int(), first[1].as_int()), (1, 10));
    assert_eq!((second[0].as_int(), second[1].as_int()), (2, 20));
}

#[test]
fn take_after_filter_counts_filtered_elements_not_source_indices() {
    let (runtime, result) = run_main(
        "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.push(5)\n  v.filter(|x| x % 2 == 0).take(2).sum()\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 6, "take(2) applies after filter");
}

#[test]
fn skip_after_filter_counts_filtered_elements_not_source_indices() {
    let (runtime, result) = run_main(
        "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.push(6)\n  v.filter(|x| x % 2 == 0).skip(1).sum()\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 10, "skip(1) drops the first filtered item");
}

#[test]
fn zip_after_filter_uses_dense_filtered_positions() {
    let (runtime, result) = run_main(
        "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  var rhs = Vec()\n  rhs.push(10)\n  rhs.push(20)\n  v.filter(|x| x % 2 == 0).zip(rhs).count()\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(
        result.as_int(),
        2,
        "both filtered elements should pair with the two-element rhs"
    );
}

#[test]
fn position_after_filter_reports_the_filtered_sequence_index() {
    let (runtime, result) = run_main(
        // The `match` is REP-39: `position` answers `Option[Int]` now, so the
        // index this test is about arrives inside a `Some`. The measurement is
        // unchanged.
        "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  match v.filter(|x| x % 2 == 0).position(|x| x == 4) { Some(i) => i, None => -1 }\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(
        result.as_int(),
        1,
        "the filtered sequence is [2, 4], so 4 is at position one"
    );
}

#[test]
fn two_flat_map_stages_compose_without_a_compiler_panic() {
    let (runtime, result) = run_main(
        "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.flat_map(|x| {\n    var a = Vec()\n    a.push(x)\n    a\n  }).flat_map(|x| {\n    var b = Vec()\n    b.push(x)\n    b\n  }).count()\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn take_after_flat_map_counts_the_global_flattened_stream() {
    let (runtime, result) = run_main(
        "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.flat_map(|x| {\n    var inner = Vec()\n    inner.push(x)\n    inner\n  }).take(1).count()\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn position_after_flat_map_uses_the_global_flattened_index() {
    let (runtime, result) = run_main(
        // As above: the index is inside a `Some` (REP-39).
        "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  match v.flat_map(|x| {\n    var inner = Vec()\n    inner.push(x)\n    inner\n  }).position(|x| x == 2) { Some(i) => i, None => -1 }\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn any_after_flat_map_short_circuits_the_whole_pipeline() {
    let (runtime, result) = run_main(
        "fn main() -> Bool {\n  var v = Vec()\n  v.push(1)\n  v.push(0)\n  v.flat_map(|x| {\n    var inner = Vec()\n    inner.push(x)\n    inner\n  }).any(|x| x == 1 || 10 / x > 0)\n}\n",
    );
    assert!(
        !runtime.has_pending_fault(),
        "the predicate must not run on 0 after any already found true: {:?}",
        runtime.fault()
    );
    assert!(result.as_bool());
}

#[test]
fn nested_record_inequality_dispatches_to_the_record_descriptor() {
    // The pre-existing tuple-of-record test only covered equal records. If the
    // tuple schema incorrectly records INT for a Record element, INT equality
    // compares the RecordPayload's first machine word (the shared schema
    // pointer) and declares every same-shaped record equal.
    let (runtime, result) = run_main(
        "struct P { x: Int }\nfn main() -> Int {\n  var a = (P { x: 1 }, 0)\n  var b = (P { x: 2 }, 0)\n  if a == b { 1 } else { 0 }\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 0);
}

#[test]
fn vec_float_push_adopts_float_descriptor_and_preserves_signed_zero_semantics() {
    // This is a passing guard, not proof that codegen selected FLOAT:
    // praxis_vec_push currently repairs the initial INT fallback by adopting
    // the first pushed value's descriptor. Keep the signed-zero behavior
    // covered while the direct empty-Vec regression below exposes codegen.
    let (runtime, result) = run_main(
        "fn main() -> Int {\n  var a = Vec()\n  a.push(0.0)\n  var b = Vec()\n  b.push(-0.0)\n  if a == b { 1 } else { 0 }\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 1, "+0.0 and -0.0 are equal Floats");
}

#[test]
fn empty_vec_float_has_the_float_element_descriptor_before_any_push() {
    let (runtime, result) =
        run_main("fn main() -> Vec[Float] {\n  var values: Vec[Float] = Vec()\n  values\n}\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    let payload = result.payload::<VecPayload>();
    // SAFETY: result is a Vec. The descriptor may be null ("never told its
    // element type"), which is read as an `Option` rather than dereferenced —
    // a null deref here aborts the whole test process.
    let descriptor = unsafe { (*payload).element() };
    assert_eq!(
        descriptor.map(|d| d.id()),
        Some(praxis_runtime::scalars::FLOAT.id()),
        "an empty Vec[Float] has no first push that can repair a wrong descriptor"
    );
}

#[test]
fn record_schema_cache_is_scoped_by_type_database_not_bare_def_id() {
    // record_def_id is positional and restarts in every independently analyzed
    // program. Use a high id so unrelated tests cannot seed it accidentally,
    // then compile two JIT generations with different shapes at the same id.
    const TARGET: usize = 211;
    let mut seed = String::new();
    for i in 0..=TARGET {
        writeln!(&mut seed, "struct Seed{i} {{ value: Int }}").unwrap();
    }
    writeln!(
        &mut seed,
        "fn main() -> Int {{ var x = Seed{TARGET} {{ value: 7 }}; x.value }}"
    )
    .unwrap();
    let (seed_runtime, seed_result) = run_main(&seed);
    assert!(!seed_runtime.has_pending_fault());
    assert_eq!(seed_result.as_int(), 7);

    let mut probe = String::new();
    for i in 0..=TARGET {
        writeln!(&mut probe, "struct Probe{i} {{ value: Text }}").unwrap();
    }
    writeln!(
        &mut probe,
        "fn main() -> Int {{\n  var a = Probe{TARGET} {{ value: \"left\" }}\n  var b = Probe{TARGET} {{ value: \"right\" }}\n  if a == b {{ 1 }} else {{ 0 }}\n}}"
    )
    .unwrap();
    let (probe_runtime, probe_result) = run_main(&probe);
    assert!(
        !probe_runtime.has_pending_fault(),
        "fault: {:?}",
        probe_runtime.fault()
    );
    assert_eq!(
        probe_result.as_int(),
        0,
        "a schema from the prior TypeDb must not make distinct Text fields compare as Ints"
    );
}

/// **Rewritten**, not un-ignored (plan §8.2). It asserted that a `Vec` key
/// stays retrievable after it is mutated, and named the two ways an
/// implementation could deliver that: "reject that state or keep key hash
/// identity stable". **D4 chose rejection**, so the program in the original
/// body no longer compiles and the property it asserted has no subject.
///
/// What replaced it is stated where it now lives: the program is refused, with
/// `Y014`, before it can run. `a_mutable_collection_is_not_a_key`
/// (`infer_tests.rs`) is the rule; this is the end-to-end fact that a program
/// which would have exposed Rust's mutated-key failure never reaches the JIT.
#[test]
fn a_mutable_collection_key_is_refused_before_it_can_be_mutated() {
    let src = "fn main() -> Int {\n  var key = Vec()\n  key.push(1)\n  var m = Map()\n  m.insert(key, 42)\n  key.push(2)\n  if m.contains(key) { 1 } else { 0 }\n}\n";
    let map = SourceMap::new();
    let file = map.intern("adversarial_audit.px", src);
    let parsed = parse(file, src);
    assert!(parsed.diagnostics.is_empty(), "the program parses");
    let analysis = analyze_root(file, &parsed.tree);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.code().to_string() == "Y014"),
        "a mutable collection is refused as a key: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn heavy_jit_loop_proves_that_automatic_collection_actually_ran() {
    // Result-only stress tests can pass without ever crossing the collection
    // threshold. This loop allocates several registered `Int`s per iteration —
    // `sum` runs to ~50M and `i` past 1024, both well outside the interned
    // small-`Int` range (`praxis_runtime::small_int`), so the counter's first
    // thousand iterations answer from the immortal table but everything after
    // that, and every partial sum, is a real allocation. With no sweep,
    // live_count stays far above the bound below; after automatic GC it is well
    // under it even though the arithmetic result is unchanged.
    let (runtime, result) = run_main(
        "fn main() -> Int {\n  var sum = 0\n  var i = 0\n  while i < 10000 {\n    sum = sum + i\n    i = i + 1\n  }\n  sum\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 49_995_000);
    assert!(
        runtime.heap().stats().live_count < 90_000,
        "the allocation stress probe did not demonstrate a sweep: {:?}",
        runtime.heap().stats()
    );
}

/// **IPR-14, and the one thing a result-only parser test cannot see.**
///
/// The interpreter's `Vec<GcRef>` intermediates were invisible to every root
/// set. What kept them alive was that the parser never paced: `parser.rs` was
/// the last caller of `Heap::alloc_unpaced`, so no collection ever ran *inside*
/// a parse. That made the intermediates a memory-growth bug and nothing worse —
/// and it is exactly why adding a safepoint before rooting them would have
/// converted it into a use-after-free (ADR-040 Decision 3, hazard H1).
///
/// This is the shape that puts collections in the middle of the assembly.
/// `scan(choice(...))` retries every case at every position, and the `mul(1,x)`
/// junk makes the first case allocate an `Int` for its first capture and *then*
/// fail — so the parse produces far more garbage than it keeps, which is what
/// paces the collector. The live intermediates being tested are `scan`'s
/// growing `items` vector and each `choice` payload held across `alloc_enum`.
///
/// Two assertions, because either alone would pass with the rooting removed.
/// The values must be compared and not merely counted: swept storage is reused
/// (the free list is keyed on layout), so a reclaimed live intermediate
/// surfaces as type confusion rather than a clean crash. And `live_count` has
/// to be far below what was allocated, or nothing was reclaimed at all.
///
/// **What this test does and does not gate.** Its subject is that pacing and
/// rooting are *consistent*: in a tree with only the two `scope.root(…)` calls
/// in `walk_scan` and `walk_choice` deleted it answers 574840 against 449400,
/// which is the number this commit reports. It is not a differential against
/// the pre-S20 base — it passes at `b2184c8`, where the parser is unpaced and
/// unrooted, so both assertions hold with no collection inside the parse at
/// all. The `live_count` guard cannot tell a sweep that ran while the parse was
/// assembling from one the generated code paced during the `for` loop
/// afterwards; measuring the parse itself would need a collection counter
/// sampled across a program that only does `read`.
#[test]
fn choice_backtracking_under_allocation_pressure_keeps_every_live_intermediate() {
    // 600 real matches; 15 allocate-then-fail attempts before each one.
    const GROUPS: usize = 600;
    const JUNK_PER_GROUP: usize = 15;
    let mut input = String::new();
    let mut expected: i64 = 0;
    for n in 0..GROUPS {
        for _ in 0..JUNK_PER_GROUP {
            // `mul(` and the first `int` both match, so the case allocates
            // before `x` defeats its second capture. That allocation is the
            // garbage this test runs on.
            input.push_str("mul(1,x) ");
        }
        if n % 2 == 0 {
            writeln!(&mut input, "dbl({n})").unwrap();
            expected += (n as i64) * 2;
        } else {
            writeln!(&mut input, "tpl({n})").unwrap();
            expected += (n as i64) * 3;
        }
    }

    let src = "fn main() -> Int {\n  \
               var ms = read scan(choice(\n    \
               M: `mul({a:int},{b:int})`,\n    \
               D: `dbl({int})`,\n    \
               T: `tpl({int})`,\n  ))\n  \
               var total = 0\n  \
               for m in ms {\n    \
               total = total + match m {\n      \
               M(p) => 0\n      \
               D(n) => n * 2\n      \
               T(n) => n * 3\n    }\n  }\n  total\n}\n";
    let (runtime, result) = run_main_with_input(src, &input);

    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(
        result.as_int(),
        expected,
        "every match's payload must survive the collections that ran during the parse"
    );
    // At least GROUPS * JUNK_PER_GROUP = 9000 objects were allocated and
    // discarded inside the parse, on top of the ~600 that survive it. Without a
    // sweep every one of them is still registered.
    let stats = runtime.heap().stats();
    assert!(
        stats.live_count < 6_000,
        "no sweep ran inside the parse, so this proves nothing about rooting: {stats:?}"
    );
}

#[test]
fn sections_preserve_text_offsets_into_the_original_input() {
    let src = "fn main() -> Text {\n  var groups = read sections(lines(word))\n  groups.get(1).get(0)\n}\n";
    let (runtime, result) = run_main_with_input(src, "alpha\n\nbeta\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_text(), "beta");
}

#[test]
fn lines_require_each_child_parser_to_consume_the_whole_line() {
    // `int` may consume the `12` prefix, but the trailing `junk` makes the line
    // invalid under §7.5 full-consumption semantics.
    let (runtime, _raw, _unit) = run_main_raw_with_input(
        "fn main() -> Int {\n  var values = read lines(int)\n  values.len()\n}\n",
        "12junk\n",
    );
    assert_eq!(
        runtime.fault(),
        praxis_runtime::FaultKind::ParseFailed,
        "a partially consumed line must not be accepted"
    );
}

#[test]
fn lines_rest_is_bounded_to_each_line() {
    let src = "fn main() -> Text {\n  var values = read lines(rest)\n  values.get(1)\n}\n";
    let (runtime, result) = run_main_with_input(src, "alpha\nbeta\ngamma\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_text(), "beta");
}

#[test]
fn anonymous_word_template_vec_uses_the_text_element_descriptor() {
    let src = "fn main() -> Vec[Text] {\n  read lines(`{word}`)\n}\n";
    let (runtime, result) = run_main_with_input(src, "alpha\nbeta\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());

    let mut rendered = String::new();
    result.format(&mut rendered);
    assert_eq!(rendered, "[alpha, beta]");

    let payload = result.payload::<VecPayload>();
    // SAFETY: result is a Vec according to both its static and runtime type.
    let descriptor = unsafe { &*(*payload).element_descriptor };
    assert_eq!(
        descriptor.id(),
        praxis_runtime::text::TEXT.id(),
        "a Vec of word captures must dispatch through TEXT"
    );
}

#[test]
fn template_text_capture_stops_before_the_following_literal() {
    let src = "fn main() -> Text {\n  var parsed = read `pre{body:text}post`\n  parsed.body\n}\n";
    let (runtime, result) = run_main_with_input(src, "premiddlepost");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_text(), "middle");
}

/// **A `grid(char)` column is positional, so a space is a cell.**
///
/// D11 made a grid's width a *cell count*, which is right; combined with
/// `walk_atomic`'s unconditional leading-horizontal-whitespace trim it meant
/// `char` skipped every space in a row and the space vanished as a cell. So
/// `grid(char)` over `"ab\na b\n"` counted two cells in both rows and answered
/// a clean 2x2 grid with `b` shifted left into the space's slot — a **wrong
/// answer** where the byte-width predecessor gave a wrong shape. A silently
/// misaligned grid is the worse of the two.
///
/// `char` now reads the scalar at the cursor. §7.4's "surrounding horizontal
/// space handled by caller" is a rule for the numeric atomics; a character
/// parser that skips spaces cannot represent one.
///
/// Every input here ends with a newline, which is the case the stage got wrong
/// everywhere else.
#[test]
fn a_grid_of_char_is_positional_so_a_space_is_a_cell() {
    // A space occupies its own column.
    let src = "fn main() -> Int {\n  var g = read grid(char)\n  g.width() * 10 + g.height()\n}\n";
    let (runtime, result) = run_main_with_input(src, "a b\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 31, "\"a b\" is three columns, not two");

    // And it is readable at the column it occupies.
    let src = "fn main() -> Char {\n  var g = read grid(char)\n  g.get(1, 0)\n}\n";
    let (runtime, result) = run_main_with_input(src, "a b\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_char(), ' ');

    // Trailing and leading spaces are cells too, for the same reason.
    let src = "fn main() -> Int {\n  var g = read grid(char)\n  g.width() * 10 + g.height()\n}\n";
    let (runtime, result) = run_main_with_input(src, "ab \n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 31);
    let (runtime, result) = run_main_with_input(src, " a\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 21);

    // A genuinely ragged grid is rejected again, which is the point: §7.5 says
    // every row has the same cell count, and this input does not.
    let (runtime, _raw, _unit) = run_main_raw_with_input(
        "fn main() -> Int {\n  var g = read grid(char)\n  g.width()\n}\n",
        "ab\na b\n",
    );
    assert_eq!(
        runtime.fault(),
        praxis_runtime::FaultKind::ParseFailed,
        "a two-cell row and a three-cell row are not one grid"
    );
}

/// **A row's trailing horizontal whitespace is padding, not a cell.**
///
/// `grid(int)` faulted on a row ending in a space while `matrix(int)` over the
/// identical file succeeded — two whitespace-token constructors disagreeing
/// about one ordinary input. §7.5 requires only that every row have the same
/// cell count.
#[test]
fn a_grid_row_may_end_in_horizontal_whitespace() {
    let src =
        "fn main() -> Int {\n  var g = read grid(int)\n  g.width() * 100 + g.height() * 10 + g[1, 1]\n}\n";
    let (runtime, result) = run_main_with_input(src, "12 34 \n56 78 \n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 2 * 100 + 2 * 10 + 78);

    // The same file through `matrix`, which never had the defect: the two
    // constructors agree now, and that agreement is the assertion.
    let src = "fn main() -> Int {\n  var g = read matrix(int)\n  g.width() * 100 + g.height() * 10 + g[1, 1]\n}\n";
    let (runtime, result) = run_main_with_input(src, "12 34 \n56 78 \n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 2 * 100 + 2 * 10 + 78);
}

/// **Whitespace is data when the parser offered it reads it — for every root
/// parser and every file ending.** A matrix, not an example, because this class
/// of defect has now been shipped three times and each time the fix was checked
/// against one example.
///
/// Round one applied `walk_exact`'s "a bounded child must fill its bound" rule
/// at a root region that ran to the end of the file, so `read ws(int)` over
/// `"1 2 3\n"` asked `int` to eat the `\n`. Round two trimmed exactly one
/// terminator off the buffer, and a file ending `"\n\n"` reproduced that
/// verbatim — `read ws(P)`, `read sep(s, P)` and `read chars(P, skip:)` faulted
/// with the identical messages, one byte later. Both treated a number of bytes
/// rather than the rule.
///
/// The rule is stated in `parser/cursor.rs`: **whitespace is data when the
/// parser offered it reads it.** The deciding half is the one that can ask —
/// `walk_exact`, `walk_characters` and `walk_grid_row` forgive a leftover run
/// through one predicate, and `trailing_blank_run` lets `lines`, `grid` — both
/// forms, uniform and ragged — and `matrix` drop a trailing blank *line* only
/// when their parser makes nothing of it. The
/// parser-independent half decides nothing any more: it drops a trailing run of
/// **empty** lines, which have no bytes to offer anyone. Together they leave the
/// terminator to nobody, so the root region is simply the whole buffer.
///
/// Round three had those two halves answering the same question differently:
/// `split_lines` deleted a trailing line of *spaces* without asking, so
/// `grid(char)` called `"ab\ncd \n"` ragged (a space is a cell) and silently
/// answered a 2x2 grid for `"ab\ncd\n  \n"` (a line of spaces is not a line).
/// The `grid(char)` block at the end of this test is the pair that disagreed,
/// asserted together.
///
/// Every root constructor §7.5 names is crossed with every ending real input
/// arrives with. Each cell asserts a **value** and not merely the absence of a
/// fault, so a rule that swallowed data instead of whitespace would fail here
/// too.
#[test]
fn every_root_parser_reads_every_file_ending() {
    // No terminator; the ordinary one; CRLF; a blank final line; a trailing
    // space before the newline; and a final line of nothing but spaces.
    const ENDINGS: [&str; 6] = ["", "\n", "\r\n", "\n\n", " \n", "\n  \n"];

    // (label, program, input body without any terminator, the one answer every
    // ending must give). The answers are shaped so a mis-split fails: a count
    // *and* an element, or a width *and* a height.
    let forms: [(&str, &str, &str, i64); 13] = [
        ("ws(int)", "read ws(int)", "1 2 3", 33),
        ("sep(\",\", int)", "read sep(\",\", int)", "1,2,3", 33),
        ("csv(int)", "read csv(int)", "1,2,3", 33),
        (
            "chars(digit, skip: none)",
            "read chars(digit, skip: none)",
            "123",
            33,
        ),
        (
            "chars(digit, skip: whitespace)",
            "read chars(digit, skip: whitespace)",
            "1 2 3",
            33,
        ),
        (
            "chars(digit, skip: newlines)",
            "read chars(digit, skip: newlines)",
            "1\n2\n3",
            33,
        ),
        ("lines(int)", "read lines(int)", "1\n2\n3", 33),
        ("lines(csv(int))", "read lines(csv(int))", "1\n2\n3", 33),
        // A template at the root, and the same template under `lines`: the
        // capture bound is region-relative, so both have to be checked.
        (
            "lines(template)",
            "read lines(`{n:int} x`)",
            "1 x\n2 x\n3 x",
            33,
        ),
        ("matrix(int)", "read matrix(int)", "1 2 3", 31),
        ("grid(digit)", "read grid(digit)", "123", 31),
        (
            "sections(lines(int))",
            "read sections(lines(int))",
            "1\n2\n3",
            33,
        ),
        (
            "sep(\" -> \", word)",
            "read sep(\" -> \", word)",
            "a -> b -> c",
            31,
        ),
    ];

    for (label, parser, body, want) in forms {
        // `len() * 10 + <a member>` for a Vec; `width() * 10 + height()` for a
        // Grid; the trailing-`x` template Vec reads its last capture.
        let tail = match label {
            "matrix(int)" | "grid(digit)" => "v.width() * 10 + v.height()",
            "lines(csv(int))" => "v.len() * 10 + v.get(2).get(0)",
            "lines(template)" => "v.len() * 10 + v.get(2).n",
            "sections(lines(int))" => "v.len() * 30 + v.get(0).get(2)",
            "sep(\" -> \", word)" => "v.len() * 10 + v.get(2).len()",
            _ => "v.len() * 10 + v.get(2)",
        };
        let src = format!("fn main() -> Int {{\n  var v = {parser}\n  {tail}\n}}\n");
        for ending in ENDINGS {
            let input = format!("{body}{ending}");
            let (runtime, result) = run_main_with_input(&src, &input);
            assert!(
                !runtime.has_pending_fault(),
                "{label} over {input:?} faulted: {:?}",
                runtime.fault()
            );
            assert_eq!(
                result.as_int(),
                want,
                "{label} over {input:?} — every ending is the same data"
            );
        }
    }

    // §7.5's own `chars(one_of("^v<>"), skip: whitespace)`, which is the
    // spelling both earlier rounds broke. Its elements are `Char`s, so the
    // measure is the count — enough here, because a terminator read as an
    // element or a value dropped both change it.
    let src = "fn main() -> Int {\n  var v = read chars(one_of(\"^v<>\"), skip: whitespace)\n  v.len()\n}\n";
    for ending in ENDINGS {
        let input = format!("^v<>{ending}");
        let (runtime, result) = run_main_with_input(src, &input);
        assert!(
            !runtime.has_pending_fault(),
            "§7.5's chars example over {input:?} faulted: {:?}",
            runtime.fault()
        );
        assert_eq!(result.as_int(), 4, "four moves, whatever ends the file");
    }

    // Two rows the matrix states rather than shares, because their right answer
    // genuinely differs — and saying so is the point of a matrix.
    //
    // `rest` consumes the remainder of its region, and at the root the region
    // is the buffer. So it reads the terminator, which is what makes
    // `parse(t, rest)` the identity on `t` — the property the round-two trim
    // broke, and the reason the trim is gone rather than merely smaller.
    let src = "fn main() -> Int {\n  var t = read rest\n  t.len()\n}\n";
    for ending in ENDINGS {
        let input = format!("abc{ending}");
        let (runtime, result) = run_main_with_input(src, &input);
        assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
        assert_eq!(
            result.as_int(),
            input.len() as i64,
            "`rest` reads its whole region, terminator included: {input:?}"
        );
    }

    // `grid(char)` is positional, so a space is a cell (ADR-079) — and this is
    // the block where the two halves of the rule used to contradict each other,
    // seventeen lines apart, one asserting each answer. They are asserted
    // together now.
    //
    // An ending with no bytes for `char` to read is nobody's, so it is the same
    // 2x2 grid…
    let src = "fn main() -> Int {\n  var g = read grid(char)\n  g.width() * 10 + g.height()\n}\n";
    for ending in ["", "\n", "\r\n", "\n\n", "\n\n\n", "\r\n\r\n"] {
        let input = format!("ab\ncd{ending}");
        let (runtime, result) = run_main_with_input(src, &input);
        assert!(
            !runtime.has_pending_fault(),
            "grid(char) over {input:?} faulted: {:?}",
            runtime.fault()
        );
        assert_eq!(result.as_int(), 22, "grid(char) over {input:?}");
    }
    // …a final line of two spaces is a **row of two cells**, because `char`
    // reads a space. This is the cell round three asserted as 22 while
    // asserting six lines below that one space on a data row is a cell: a line
    // of spaces was deleted by `split_lines` before `char` was asked. One
    // question, one answer — the child's.
    for ending in ["\n  \n", "\n  \n\n", "\n  "] {
        let input = format!("ab\ncd{ending}");
        let (runtime, result) = run_main_with_input(src, &input);
        assert!(
            !runtime.has_pending_fault(),
            "grid(char) over {input:?} faulted: {:?}",
            runtime.fault()
        );
        assert_eq!(
            result.as_int(),
            23,
            "grid(char) over {input:?} — two spaces `char` reads are a third row"
        );
    }
    // A grid of nothing but spaces is that grid, not an empty one. Round three
    // answered width 0, height 0 here — four cells silently deleted.
    let (runtime, result) = run_main_with_input(src, "  \n  \n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(
        result.as_int(),
        22,
        "\"  \\n  \\n\" is a 2x2 grid of spaces"
    );
    // The children that cannot read a space read none of it, and for them the
    // same file ending is nobody's: `grid(digit)`, `matrix(int)` and
    // `lines(int)` are unmoved by it. That difference is the rule working, not
    // the constructors disagreeing.
    let digits =
        "fn main() -> Int {\n  var g = read grid(digit)\n  g.width() * 10 + g.height()\n}\n";
    let (runtime, result) = run_main_with_input(digits, "12\n34\n  \n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 22, "`digit` reads no cell in \"  \"");
    // …and a file whose last row alone carries a trailing space is a **ragged
    // grid**. That is a complaint about the data, and a different complaint
    // from "the rest of the line": put the space on every row and the grid is
    // simply one column wider.
    let (runtime, _raw, _unit) = run_main_raw_with_input(src, "ab\ncd \n");
    assert_eq!(
        runtime.fault(),
        praxis_runtime::FaultKind::ParseFailed,
        "a two-cell row and a three-cell row are not one grid, whatever the third cell is"
    );
    let (runtime, result) = run_main_with_input(src, "ab \ncd \n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 32, "every row three cells wide");
    // A tab is a character exactly as a space is, so a final line of `" \t "`
    // is a **three-cell row** and the grid is ragged — where the two-space
    // ending five lines above is a two-cell row that fits. The commit that
    // moved this cell named it and nothing end-to-end held it: the ragged
    // direction was pinned only for the all-spaces shape and the tab only at
    // the substrate, so a regression that made a tab behave unlike a space
    // would have been caught only indirectly.
    let (runtime, _raw, _unit) = run_main_raw_with_input(src, "ab\ncd\n \t \n");
    assert_eq!(
        runtime.fault(),
        praxis_runtime::FaultKind::ParseFailed,
        "\" \\t \" is three cells against a two-cell grid"
    );

    // `lines(rest)` is lossless, which is `rest`'s identity property one level
    // up: round three's extent trim deleted the third line for every child,
    // including the children that read it.
    let src =
        "fn main() -> Int {\n  var v = read lines(rest)\n  v.len() * 10 + v.get(1).len()\n}\n";
    let (runtime, result) = run_main_with_input(src, "ab\ncd\n  \n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 32, "`rest` reads \"  \", so it is a line");

    // **A capture answers from the rule too** — the last construct that did
    // not. `skip_capture_ws` used to move the cursor before the child was ever
    // offered the bytes, so the same child on the same file answered one way
    // bare and another inside a template: silently for `char` (two elements,
    // not three), as a hard fault for the interior blank line, and as lost
    // bytes for `text`/`rest`. The only template row in the matrix above is
    // `` `{n:int} x` ``, whose child cannot read whitespace, so the skip was
    // invisible to the whole suite. Each pair is asserted together, so the two
    // spellings cannot drift apart again.
    let pairs: [(&str, &str, &str, i64); 4] = [
        // A trailing line of two spaces is one `char` cell, so it is an element.
        (
            "lines(char) / lines(`{a:char}`) over a trailing blank line",
            "read lines(char)",
            "read lines(`{a:char}`)",
            3,
        ),
        // The same rule at an *interior* blank line, where the skip did not
        // merely lose an element — it faulted where the bare spelling read.
        (
            "lines(char) / lines(`{a:char}`) over an interior blank line",
            "read lines(char)",
            "read lines(`{a:char}`)",
            3,
        ),
        // `lines(rest)` is lossless and so is the capture spelling of it.
        (
            "lines(rest) / lines(`{a:rest}`) last element bytes",
            "read lines(rest)",
            "read lines(`{a:rest}`)",
            2,
        ),
        // Ditto `text`, which is `rest` with a bound and no bound here.
        (
            "lines(text) / lines(`{a:text}`) last element bytes",
            "read lines(text)",
            "read lines(`{a:text}`)",
            2,
        ),
    ];
    for (n, (label, bare, capture, want)) in pairs.iter().enumerate() {
        let (input, bare_tail, capture_tail) = match n {
            0 => ("x\ny\n  \n", "v.len()", "v.len()"),
            1 => ("x\n  \ny\n", "v.len()", "v.len()"),
            _ => (
                "x\ny\n  \n",
                "v.get(v.len() - 1).len()",
                "v.get(v.len() - 1).a.len()",
            ),
        };
        for (parser, tail, which) in [
            (bare, bare_tail, "bare"),
            (capture, capture_tail, "capture"),
        ] {
            let src = format!("fn main() -> Int {{\n  var v = {parser}\n  {tail}\n}}\n");
            let (runtime, result) = run_main_with_input(&src, input);
            assert!(
                !runtime.has_pending_fault(),
                "{label} ({which}) over {input:?} faulted: {:?}",
                runtime.fault()
            );
            assert_eq!(
                result.as_int(),
                *want,
                "{label} ({which}) over {input:?} — one child, one answer"
            );
        }
    }
    // A capture at the root, with no `lines` in the picture: `rest` and `text`
    // read their leading whitespace, and wrapping them in a capture does not
    // take it away.
    for parser in ["rest", "text"] {
        let bare = format!("fn main() -> Int {{\n  var t = read {parser}\n  t.len()\n}}\n");
        let capture =
            format!("fn main() -> Int {{\n  var r = read `{{a:{parser}}}`\n  r.a.len()\n}}\n");
        for (src, which) in [(&bare, "bare"), (&capture, "capture")] {
            let (runtime, result) = run_main_with_input(src, " ab\n");
            assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
            assert_eq!(
                result.as_int(),
                4,
                "`{parser}` ({which}) reads its leading space"
            );
        }
    }
    // The bound scan still starts past the capture's own leading whitespace,
    // which is a bound question and not a whitespace-reading one. Without that
    // offset the following literal run — `SpaceRun` plus empty text — matches
    // the indent itself, `n` is bounded at byte 0 and `int` is handed the word.
    let src =
        "fn main() -> Int {\n  var v = read lines(`{n:int} x`)\n  v.len() * 10 + v.get(2).n\n}\n";
    let (runtime, result) = run_main_with_input(src, " 1 x\n 2 x\n 3 x\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 33, "an indented template still matches");

    // **A csv field answers from the rule too.** `csv_tokens` used to
    // `str::trim()` every field, which decided about whitespace without asking
    // the field's parser — and `trim()` eats vertical whitespace, so `csv(rest)`
    // lost the terminator `sep(",", rest)` keeps. §7.5's "ignore horizontal
    // whitespace around each comma" now falls out of the rule instead: `int`
    // skips it, `char` reads it, and neither is told which.
    for (label, child, input, tail, want) in [
        (
            "rest keeps the terminator",
            "rest",
            "a,b,c\n",
            "v.get(2).len()",
            2,
        ),
        (
            "text keeps the leading space",
            "text",
            "a, b,c\n",
            "v.get(1).len()",
            2,
        ),
        (
            "char reads a space as a field",
            "char",
            "a, ,c\n",
            "v.len()",
            3,
        ),
        (
            "int still skips its padding",
            "int",
            " 1, 2, 3\n",
            "v.len() * 10 + v.get(2)",
            33,
        ),
    ] {
        let csv = format!("fn main() -> Int {{\n  var v = read csv({child})\n  {tail}\n}}\n");
        let sep =
            format!("fn main() -> Int {{\n  var v = read sep(\",\", {child})\n  {tail}\n}}\n");
        for (src, which) in [(&csv, "csv"), (&sep, "sep(\",\", …)")] {
            let (runtime, result) = run_main_with_input(src, input);
            assert!(
                !runtime.has_pending_fault(),
                "{label} ({which}) over {input:?} faulted: {:?}",
                runtime.fault()
            );
            assert_eq!(
                result.as_int(),
                want,
                "{label} ({which}) over {input:?} — one rule, two constructors"
            );
        }
    }
}

/// **An interior blank line is structure, and no constructor gets to skip
/// one.**
///
/// `matrix` did: `walk_matrix` skipped every line that trimmed to nothing, so
/// `matrix(int)` over `"1 2\n  \n3 4\n"` silently answered a 2x2 grid while
/// `lines(int)` and `grid(digit)` faulted on the identical shape. Three
/// constructs, three answers, for the one rule they are all supposed to inherit
/// — and the skip is precisely the per-constructor whitespace special case
/// ADR-078's corollary tells a later contributor not to write.
///
/// The trailing case is the other half of the pair and is *not* a special case:
/// a trailing blank line is offered, and belongs to nobody when the parser makes
/// nothing of it.
#[test]
fn an_interior_blank_line_is_a_row_and_a_trailing_one_is_nobodys() {
    let matrix =
        "fn main() -> Int {\n  var g = read matrix(int)\n  g.width() * 10 + g.height()\n}\n";
    let lines = "fn main() -> Int {\n  var v = read lines(int)\n  v.len()\n}\n";
    let grid = "fn main() -> Int {\n  var g = read grid(digit)\n  g.width() * 10 + g.height()\n}\n";

    // Interior: all three complain, none silently deletes a line.
    let (runtime, _raw, _unit) = run_main_raw_with_input(matrix, "1 2\n  \n3 4\n");
    assert_eq!(
        runtime.fault(),
        praxis_runtime::FaultKind::ParseFailed,
        "a zero-token row is not two tokens wide"
    );
    let (runtime, _raw, _unit) = run_main_raw_with_input(lines, "1\n  \n2\n");
    assert_eq!(runtime.fault(), praxis_runtime::FaultKind::ParseFailed);
    let (runtime, _raw, _unit) = run_main_raw_with_input(grid, "12\n  \n34\n");
    assert_eq!(runtime.fault(), praxis_runtime::FaultKind::ParseFailed);

    // Trailing: all three read the same data, because none of their parsers
    // makes anything of a line of spaces.
    let (runtime, result) = run_main_with_input(matrix, "1 2\n3 4\n  \n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 22);
    let (runtime, result) = run_main_with_input(lines, "1\n2\n  \n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 2);
    let (runtime, result) = run_main_with_input(grid, "12\n34\n  \n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 22);

    // **A child that succeeds vacuously has made something of the line**, and
    // that is where `matrix(P)` and `lines(ws(P))` part company. `ws(int)`
    // answers an all-whitespace region with an *empty* `Vec` rather than a
    // failure, so it makes an element and `walk_lines` keeps the line — three
    // elements over the bytes `matrix(int)` reads as a 2x2 grid, because
    // `matrix` has no zero-token row to make. Same criterion, two children,
    // and §7.5 says so rather than leaving a reader to find it: `matrix` is
    // "lines containing whitespace-separated elements" and that is not a
    // definition of `lines(ws(...))`.
    let lines_ws = "fn main() -> Int {\n  var v = read lines(ws(int))\n  \
                    v.len() * 10 + v.get(v.len() - 1).len()\n}\n";
    let (runtime, result) = run_main_with_input(lines_ws, "1 2\n3 4\n  \n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(
        result.as_int(),
        30,
        "three elements, the last of them empty — `ws` made one of \"  \""
    );
    let (runtime, result) = run_main_with_input(matrix, "1 2\n3 4\n  \n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 22, "`matrix` made no row of \"  \"");
    // A final *empty* line is a third answer again, and the reason is the other
    // half of the rule: `split_lines` drops lines with no bytes to offer
    // anyone, so `ws` is never asked.
    let (runtime, result) = run_main_with_input(lines_ws, "1 2\n3 4\n\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(
        result.as_int(),
        22,
        "an empty final line is nobody's before any parser runs"
    );
    // `csv` is not in the vacuous-success list: it always makes at least one
    // field, so `csv(int)` fails on a blank line and the line is dropped.
    let lines_csv = "fn main() -> Int {\n  var v = read lines(csv(int))\n  v.len()\n}\n";
    let (runtime, result) = run_main_with_input(lines_csv, "1,2\n3,4\n  \n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 2, "`csv(int)` made nothing of \"  \"");
}

/// **One template, two constructs, one answer** (ADR-090).
///
/// `` `items: {items:csv(int)}` `` read two ints as a `lines` element and
/// swallowed the following line as a `block` item, faulting `at input offset
/// 13..23: expected the rest of the field`. `lines` had narrowed to a line;
/// `block` — the only sequencing construct that computed no window — had
/// narrowed to nothing, and the unbounded-last-part rule did the rest. That is
/// ADR-078's own defect class: two constructs in one stage disagreeing about
/// one byte.
///
/// **Asserted as a pair on purpose, against each other rather than a constant.**
/// A gate that measured only one side is what let this survive: the `lines` half
/// is 2 on both binaries.
///
/// **Observed red**: with `block_item_window` removed from `walk_block`'s two
/// call sites, the `block` half faults `ParseFailed` at `13..23`; the `lines`
/// half is unchanged.
#[test]
fn the_same_template_reads_the_same_bytes_under_lines_and_under_block() {
    let lines = "fn main() -> Int {\n  var v = read lines(`items: {items:csv(int)}`)\n  \
                 v.get(0).items.len()\n}\n";
    let block = "fn main() -> Int {\n  var b = read block(`items: {items:csv(int)}`, \
                 `op: {op:word}`)\n  b.items.len()\n}\n";

    let (runtime, from_lines) =
        run_main_with_input(lines, "items: 79, 98\nitems: 54, 65, 75, 74\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    let (runtime, from_block) = run_main_with_input(block, "items: 79, 98\nop: plus\n");
    assert!(
        !runtime.has_pending_fault(),
        "the same template under `block` faulted: {:?}",
        runtime.fault()
    );
    assert_eq!(
        from_block.as_int(),
        from_lines.as_int(),
        "one template, two constructs, one answer"
    );
    assert_eq!(
        from_lines.as_int(),
        2,
        "and the answer is the line's two ints"
    );
}

/// **A whitespace-only template part is a bound too.**
///
/// The capture bound first looked only for a following literal with *non-empty*
/// text, so a capture followed by a plain space run — the most ordinary
/// template shape there is — was not bounded at all: `text` swallowed the rest
/// of its region and the space run then had nothing left to match.
/// `` lines(`{name:text} {v:int}`) `` over `"foo 3\n"` reported "expected
/// whitespace" at the end of the line. §7.9 makes a run of whitespace a
/// `Literal` whose text is empty and whose policy carries the requirement, and
/// §7.4's "until the following template literal can match" does not exempt it.
///
/// All three spellings, and all three inputs end with a newline: this defect
/// was found on a real file, and the same template inside `lines` is bounded
/// per line while the bare one is bounded by the root region.
#[test]
fn a_capture_is_bounded_by_a_whitespace_only_template_part() {
    // A plain space run. `text` is non-greedy, so the bound is the *earliest*
    // position the space run can match — the same rule a literal bound follows.
    let src =
        "fn main() -> Text {\n  var r = read lines(`{name:text} {v:int}`)\n  r.get(1).name\n}\n";
    let (runtime, result) = run_main_with_input(src, "foo 3\nbar 12\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_text(), "bar");

    // `\s+`.
    let src = "fn main() -> Int {\n  var r = read lines(`{name:text}\\s+{v:int}`)\n  var t = 0\n  for x in r { t = t + x.v }\n  t\n}\n";
    let (runtime, result) = run_main_with_input(src, "foo 3\nbar 12\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 15);

    // `\n`, which bounds a capture across a line ending.
    let src = "fn main() -> Int {\n  var r = read `{name:text}\\n{v:int}`\n  r.v\n}\n";
    let (runtime, result) = run_main_with_input(src, "foo\n3\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 3);

    // `\s*` still constrains nothing, so it is still skipped: the scan looks
    // past it for something that does. Nothing does here, so the capture is
    // unbounded and `text` takes the line — which is the documented answer for
    // a template that asks for zero-or-more.
    let src = "fn main() -> Text {\n  var r = read lines(`{name:text}\\s*`)\n  r.get(0).name\n}\n";
    let (runtime, result) = run_main_with_input(src, "foo 3\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_text(), "foo 3");

    // A literal the template wrote with **nothing** in front of it carries
    // `WsPolicy::None`, so it is not a run and it absorbs nothing. Both halves
    // are pinned here because ADR-079 Decision 2 and `capture_bound`'s doc
    // comment used to explain this case by crediting "the comma's `SpaceRun`"
    // with eating the space, a mechanism two later decisions removed.
    let src = "fn main() -> Int {\n  var r = read `{a:int},{b:int}`\n  r.a * 100 + r.b\n}\n";
    // No space at all — which a one-or-more `SpaceRun` could not match.
    let (runtime, result) = run_main_with_input(src, "12,34\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 1234);
    // A space before the comma. The bound is where the run can start, so it is
    // the comma at byte 3; `a` is handed "12 " and does NOT fill it. The space
    // is forgiven by ADR-078's rule — whitespace the parser offered it did not
    // read — which is the same rule `lines`, `grid` and `ws` answer from.
    let (runtime, result) = run_main_with_input(src, "12 ,34\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 1234);
}

/// **`parse(t, rest)` is the identity on `t`.**
///
/// `run_plan` is the single body behind both `read <parser>` and the host
/// `parse(text, P)`, and a repair pass trimmed the input file's trailing
/// terminator inside it. That is a file convention, and the second caller has no
/// file: the `\n` a *program* wrote into its own literal was deleted, so
/// `parse("abc\n", rest)` and `parse("abc", rest)` answered the same `Text` and
/// nothing could recover the difference. §7.4 defines `rest` as "consumes the
/// remainder of the current region", and at the root the region is the text the
/// caller handed in.
///
/// The trim is gone rather than narrowed to the `read` path: nothing needed it
/// once trailing whitespace was left to nobody by rule (ADR-078). This is the
/// property that says so, and it is the one a re-introduced trim of *any* size
/// would break.
#[test]
fn a_parse_is_the_identity_on_the_text_it_was_given() {
    let src = "fn main() -> Int {\n  \
               var a = parse(\"abc\\n\", rest)\n  \
               var b = parse(\"abc\", rest)\n  \
               var c = parse(\"abc\\r\\n\", rest)\n  \
               var d = parse(\"abc\\n\\n\", rest)\n  \
               a.len() * 1000 + b.len() * 100 + c.len() * 10 + d.len()\n}\n";
    let (runtime, result) = run_main_with_input(src, "unrelated stdin\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(
        result.as_int(),
        4 * 1000 + 3 * 100 + 5 * 10 + 5,
        "`parse` reads the Text it was handed, terminators included"
    );

    // The same at the `read` end, so the two callers are visibly one function:
    // a root `rest` takes the whole input file, terminator included.
    let src = "fn main() -> Int {\n  var t = read rest\n  t.len()\n}\n";
    let (runtime, result) = run_main_with_input(src, "abc\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 4);
}

/// **Two spellings of one whitespace policy bound a capture alike.**
///
/// `` lines(`{a:text} bar`) `` read `"x y bar"` as `a = "x y"` while
/// `` lines(`{a:text}\s+bar`) `` over the identical bytes faulted with
/// `expected literal "bar"` at the interior space. §7.9 lowers `\s+` to its own
/// empty-text part, and the bound was computed from the *first* following part
/// that constrains anything — which for the escaped spelling is the space run
/// alone, and a space run matches at the first space, where `bar` is not.
///
/// §7.4 says `text` "minimally consumes text until the following template
/// literal can match". What has to be able to match is the whole run of parts
/// before the next capture, which is the only reading under which two spellings
/// of one policy agree. Both directions are pinned: the case that was already
/// right must stay right.
#[test]
fn two_spellings_of_one_whitespace_policy_bound_a_capture_alike() {
    let escaped = "fn main() -> Text {\n  var r = read lines(`{a:text}\\s+bar`)\n  r.get(0).a\n}\n";
    let plain = "fn main() -> Text {\n  var r = read lines(`{a:text} bar`)\n  r.get(0).a\n}\n";

    // Past an interior space — the half that faulted.
    for src in [escaped, plain] {
        let (runtime, result) = run_main_with_input(src, "x y bar\n");
        assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
        assert_eq!(result.as_text(), "x y");
    }
    // No interior space — the half that already worked, and must keep working.
    for src in [escaped, plain] {
        let (runtime, result) = run_main_with_input(src, "x bar\n");
        assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
        assert_eq!(result.as_text(), "x");
    }

    // The run is matched as a run, so a leading `\s*` that constrains nothing
    // on its own still keeps the whitespace out of the capture: the bound is
    // the earliest position where `\s*bar` matches, which is before the spaces.
    let src = "fn main() -> Text {\n  var r = read lines(`{a:text}\\s*bar`)\n  r.get(0).a\n}\n";
    let (runtime, result) = run_main_with_input(src, "x  bar\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_text(), "x");
}

/// **IPR-11 without growing `word`'s delimiter set.**
///
/// `word` stops on space, tab, `,`, `\n` and `\r` and nothing else, so
/// `{w:word}-to-{x:word}` let the first `word` swallow the `-to-`. The audit's
/// reading was that the delimiter set is too small. It is not: growing it to
/// "every template delimiter" breaks `sep(" -> ", word)` and any `word` that
/// legitimately contains a `-`. What was missing is the *region* — a capture
/// bounded by the literal that follows it stops there whatever its own
/// delimiter rule says, so the set stays minimal and documented.
///
/// Both halves are pinned here: the bounded `word` stops at the literal, and a
/// bare `ws(word)` still reads `a-b` as one word.
#[test]
fn a_bounded_word_capture_stops_at_its_region_end() {
    // The shape from tests/aoc-corpus/m9_almanac.px.
    let src = "fn main() -> Text {\n  var r = read `{w:word}-to-{x:word} map:`\n  \
               r.w\n}\n";
    let (runtime, result) = run_main_with_input(src, "seed-to-soil map:");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(
        result.as_text(),
        "seed",
        "the following literal is the bound; `-` is not a word delimiter"
    );

    let src = "fn main() -> Text {\n  var r = read `{w:word}-to-{x:word} map:`\n  \
               r.x\n}\n";
    let (runtime, result) = run_main_with_input(src, "seed-to-soil map:");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_text(), "soil");

    // Unbounded, `-` is an ordinary word character and must stay one.
    let src = "fn main() -> Text {\n  var v = read ws(word)\n  v.get(0)\n}\n";
    let (runtime, result) = run_main_with_input(src, "a-b c");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(
        result.as_text(),
        "a-b",
        "a bare `word` keeps its minimal delimiter set"
    );
}

/// **IPR-07 and D-S20-A.** A collection's element descriptor and the objects it
/// holds must be the same type.
///
/// AMENDED (S20). The declared return type was `Vec[Char]`, because
/// `synthesize` hardcoded `chars(P, skip:) -> Vec[Char]` regardless of `P` —
/// which is precisely the disagreement this test exists to catch, written into
/// the test's own source. `chars(int, skip: none)` produces `Int` objects, so
/// its type is `Vec[Int]`; the static type is derived from the child now, the
/// runtime descriptor is derived from the same child, and the annotation says
/// what the program actually returns. `chars(one_of("LR"))` is still
/// `Vec[Char]`, because `one_of` synthesizes `Char`.
#[test]
fn chars_result_descriptor_matches_the_values_it_contains() {
    let src = "fn main() -> Vec[Int] {\n  read chars(int, skip: none)\n}\n";
    let (runtime, result) = run_main_with_input(src, "65");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    let payload = result.payload::<VecPayload>();
    // SAFETY: result is a runtime Vec.
    let payload = unsafe { &*payload };
    assert_eq!(payload.items.len(), 1);
    let declared = unsafe { &*payload.element_descriptor }.id();
    let actual = payload.items[0].descriptor().id();
    assert_eq!(
        declared, actual,
        "collection descriptors and stored object headers must agree"
    );
}

#[test]
fn one_generic_function_is_instantiated_at_int_and_text_in_one_program() {
    // The pre-existing test named "two clones" calls `id` twice at Int and
    // therefore proves clone reuse, not distinct monomorphic instantiations.
    let (runtime, result) = run_main(
        "fn id(x) { x }\nfn main() -> Int {\n  var word = id(\"four\")\n  id(38) + word.len()\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 42);
}

// The tests below are ignored because the current implementation can dispatch
// through a descriptor for the wrong payload shape (or return an invalid raw
// GcRef). They are still executable, focused handover regressions; keeping them
// out of the default process prevents a known bug from turning a test failure
// into host-language undefined behavior.

#[test]
fn tuple_schema_uses_the_unit_descriptor_for_unit_elements() {
    // `push` returns Unit, which is the shortest Unit-valued expression the
    // grammar accepts today: a `{ ... }` block in statement position is read as
    // a call of the following parenthesized expression until FE-04 lands (S12).
    let (runtime, result) = run_main(
        "fn main() {\n  var xs = Vec()\n  var nothing = xs.push(1)\n  var pair = (nothing, 7)\n  pair\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(
        tuple_element_descriptor_ids(result),
        vec![
            praxis_runtime::scalars::UNIT.id(),
            praxis_runtime::scalars::INT.id()
        ],
        "format/equals/hash must never read Unit's zero-sized payload as an Int"
    );
}

#[test]
fn tuple_schema_uses_the_enum_descriptor_for_enum_elements() {
    let (runtime, result) = run_main("enum Marker { A, B }\nfn main() {\n  (A, 7)\n}\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(
        tuple_element_descriptor_ids(result),
        vec![
            praxis_runtime::enums::ENUM.id(),
            praxis_runtime::scalars::INT.id()
        ],
        "enum payloads require ENUM format/equality/hash dispatch"
    );
}

#[test]
fn grid_positions_vec_uses_the_point_tuple_descriptor() {
    let (runtime, result) = run_main_with_input(
        "fn main() {\n  var g = read grid(char)\n  g.positions()\n}\n",
        "ab\ncd\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    let payload = result.payload::<VecPayload>();
    // SAFETY: the source and runtime result are both Vec values. Merely
    // inspecting the descriptor does not reinterpret any tuple payload.
    let descriptor = unsafe { &*(*payload).element_descriptor };
    assert_eq!(
        descriptor.id(),
        praxis_runtime::tuples::TUPLE.id(),
        "positions/neighbors/find_all must return Vec[(Int, Int)]"
    );
}

#[test]
fn grid_text_row_preserves_the_grid_cell_descriptor() {
    let (runtime, result) = run_main_with_input(
        "fn main() {\n  var g = read matrix(word)\n  g.row(0)\n}\n",
        "alpha beta\ngamma delta\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    let payload = result.payload::<VecPayload>();
    // SAFETY: result is a Vec. This avoids formatting the Text elements
    // through the currently incorrect Int descriptor.
    let descriptor = unsafe { &*(*payload).element_descriptor };
    assert_eq!(
        descriptor.id(),
        praxis_runtime::text::TEXT.id(),
        "row/cells/column must preserve Grid[T]'s T descriptor"
    );
}

/// S18 exit criterion (RT-15). `Grid.find`'s static type is now
/// `Option[(Int, Int)]`, so a miss is a `None` — a value of the type the
/// program was promised — rather than the Unit sentinel wearing a tuple type.
///
/// The assertion was `== TUPLE.id()` while the test was ignored, which was the
/// best a point-typed row could ask for: "whatever comes back, it must at least
/// be the shape the signature claims". D1 changed the signature, so the test
/// states the contract instead — the program matches on the answer, and both
/// arms are reachable from source.
#[test]
fn absent_grid_find_has_no_unit_under_a_tuple_type() {
    let src = "fn main() -> Int {\n\
               \x20 var g = read matrix(int)\n\
               \x20 match g.find(99) {\n    Some(p) => 1,\n    None => 0,\n  }\n\
               }\n";
    let (runtime, result) = run_main_with_input(src, "1 2\n3 4\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(
        result.as_int(),
        0,
        "nothing matched, so the answer is `None`"
    );

    let src = "fn main() -> Int {\n\
               \x20 var g = read matrix(int)\n\
               \x20 match g.find(4) {\n    Some(p) => 1,\n    None => 0,\n  }\n\
               }\n";
    let (runtime, result) = run_main_with_input(src, "1 2\n3 4\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 1, "a hit is `Some((x, y))`");

    // And the `Option` really is an enum value, not a tuple that happens to
    // answer: the descriptor is what `format`/`equals`/`hash` dispatch through.
    let (runtime, result) = run_main_with_input(
        "fn main() {\n  var g = read matrix(int)\n  g.find(99)\n}\n",
        "1 2\n3 4\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(
        result.descriptor().id(),
        praxis_runtime::enums::ENUM.id(),
        "a value statically typed Option[(Int, Int)] cannot be the Unit sentinel"
    );
}

/// S18 exit criterion (RT-14). The same for `Map.get`, whose result type is now
/// `Option[V]` (§5.7 spelled that signature all along).
///
/// The assertion was `== INT.id()` while the test was ignored, for the same
/// reason as its `Grid.find` sibling; it is a source-level unwrap now, so what
/// it pins is the language rule rather than a descriptor id.
#[test]
fn absent_map_get_has_no_unit_under_the_value_type() {
    let unwrap = "fn unwrap(o: Option[Int]) -> Int {\n  match o {\n    Some(v) => v,\n    None => 0 - 1,\n  }\n}\n";
    let (runtime, result) = run_main(&format!(
        "{unwrap}fn main() -> Int {{\n  var m = Map()\n  m.insert(1, 10)\n  unwrap(m.get(2))\n}}\n"
    ));
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), -1);
    let (runtime, result) = run_main(&format!(
        "{unwrap}fn main() -> Int {{\n  var m = Map()\n  m.insert(1, 10)\n  unwrap(m.get(1))\n}}\n"
    ));
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 10);

    let (runtime, result) =
        run_main("fn main() {\n  var m = Map()\n  m.insert(1, 10)\n  m.get(2)\n}\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(
        result.descriptor().id(),
        praxis_runtime::enums::ENUM.id(),
        "a value statically typed Option[Int] cannot be the Unit sentinel"
    );
}

#[test]
fn empty_element_returning_sinks_fault_instead_of_returning_uninitialized_gc_refs() {
    let cases = [
        (
            "reduce",
            "fn main() -> Int {\n  var v = Vec()\n  v.reduce(|a, x| a + x)\n}\n",
        ),
        (
            "min_by",
            "fn main() -> Int {\n  var v = Vec()\n  v.min_by(|a, b| a < b)\n}\n",
        ),
        (
            "max_by",
            "fn main() -> Int {\n  var v = Vec()\n  v.max_by(|a, b| a < b)\n}\n",
        ),
    ];

    for (name, source) in cases {
        // Use the integer return channel so the current zero/uninitialized
        // result is never materialized as Rust's NonNull-backed GcRef.
        let (runtime, raw_result, unit_addr) = run_main_raw_with_input(source, "");
        assert_eq!(
            runtime.fault(),
            praxis_runtime::FaultKind::EmptyCollection,
            "empty {name} needs a defined failure contract"
        );
        assert_eq!(
            raw_result, unit_addr,
            "the {name} fault path must return the valid Unit sentinel"
        );
    }
}

#[test]
fn text_ordering_is_lexicographic_without_payload_reinterpretation() {
    let (runtime, result) = run_main("fn main() -> Bool {\n  \"apple\" < \"banana\"\n}\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert!(result.as_bool());
}

/// P0-12's other half for `Text`: the eight-byte payload load compared the
/// `TextPayload` *discriminant*, so every pair of owned strings was equal and
/// every owned/slice pair was not. Equality has to move to the descriptor with
/// ordering, or `"apple" < "banana"` and `"apple" == "banana"` disagree about
/// what a `Text` is.
#[test]
fn text_equality_compares_bytes_not_the_payload_discriminant() {
    let (runtime, result) = run_main("fn main() -> Bool {\n  \"apple\" == \"banana\"\n}\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert!(
        !result.as_bool(),
        "two different strings must not be equal because both are `Owned`"
    );

    let (runtime, result) = run_main("fn main() -> Bool {\n  \"apple\" == \"apple\"\n}\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert!(result.as_bool());
}

/// **Rewritten**, not merely un-ignored: its input changed from `aβ\n` to
/// `ab\n`.
///
/// The non-ASCII cell was unparseable for a reason that has nothing to do with
/// ordering — the `grid(char)` cell parser worked in bytes, so `β` was an
/// "expected char" mismatch — and `read grid(char)` was then the only source of
/// `Char` values in the language. That is an input-parser defect, S19/S20's
/// territory alongside IPR-06's `grid(int)` granularity, and leaving it here
/// would have made an ordering test go red until a parser fix landed.
///
/// **Both halves of that reason have since expired.** S20 made the cell parser
/// read Unicode scalars, so `read grid(char)` over `aβ` answers `β` today; and
/// ADR-086 made `t[i]` answer a `Char`, so `"β"[0]` writes one. The input is
/// left at `ab\n` regardless: what this test owns is the four-byte payload
/// width, which `ab` reaches, and rewriting it to chase a reason that no longer
/// applies would change what it pins.
///
/// What survives is the property P0-12 owns and this input still reaches: two
/// `Char` cells are ordered by their four-byte payloads, not by eight bytes
/// read from one. `small_scalars_are_extracted_at_their_own_width` pins the
/// lowering that makes it so, and
/// `char_heap_entries_order_by_unicode_scalar_value` (praxis-runtime) covers
/// the non-ASCII values the parser cannot yet deliver.
#[test]
fn char_ordering_uses_unicode_scalar_values_without_out_of_bounds_reads() {
    let (runtime, result) = run_main_with_input(
        "fn main() -> Bool {\n  var g = read grid(char)\n  g.get(0, 0) < g.get(1, 0)\n}\n",
        "ab\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert!(result.as_bool());
}

/// P0-08: `Int` arithmetic lowers natively.
///
/// The old shape boxed both operands with `praxis_alloc_int`, called the
/// wrapper, and `praxis_int_load`ed the result. That `int_load` ran *before*
/// the fault check, so on overflow it read eight bytes past the size-0 Unit
/// payload the wrapper returned. Asserting on the emitted symbols is what keeps
/// the pair from coming back.
#[test]
fn int_arithmetic_emits_no_boxing_wrappers() {
    let src = "fn main() -> Int { 2 + 3 * 4 - 1 }";
    let names = runtime_symbols_emitted_for(src);
    for boxed in [
        "praxis_alloc_int",
        "praxis_int_load",
        "praxis_int_add",
        "praxis_int_sub",
        "praxis_int_mul",
    ] {
        assert!(
            !names.contains(boxed),
            "arithmetic still routes through `{boxed}`: {names:?}"
        );
    }
    let (runtime, result) = run_main(src);
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 13);
}

/// Native lowering must fault exactly where the wrappers did (§4.12).
#[test]
fn native_arithmetic_faults_match_the_runtime_wrappers() {
    for (src, kind) in [
        (
            "fn main() -> Int { 9223372036854775807 + 1 }",
            praxis_runtime::FaultKind::IntOverflow,
        ),
        (
            "fn main() -> Int { 0 - 9223372036854775807 - 2 }",
            praxis_runtime::FaultKind::IntOverflow,
        ),
        (
            "fn main() -> Int { 4294967296 * 4294967296 }",
            praxis_runtime::FaultKind::IntOverflow,
        ),
        (
            "fn main() -> Int { 1 / 0 }",
            praxis_runtime::FaultKind::DivByZero,
        ),
        (
            "fn main() -> Int { 1 % 0 }",
            praxis_runtime::FaultKind::DivByZero,
        ),
    ] {
        let (runtime, _) = run_main(src);
        assert_eq!(runtime.fault(), kind, "{src}");
    }
}

/// ADR-102: the diamond does not move the point at which a fault is observed.
///
/// The overflow report is a `brif` to a cold block now, not an unconditional
/// call, and the `Inst::CheckFault` that MIR requires next lowers into the block
/// both arms converge on. If it did not — if the diversion landed anywhere but
/// immediately after the raise — the *second* statement here would run and
/// `set_fault` would overwrite `IntOverflow` with `DivByZero`, because raising
/// is a store and nothing about it is conditional on the slot being clear.
///
/// So the fault kind is the assertion, and it is a sharper one than "it
/// faulted": it says which operation was the last one to run.
#[test]
fn an_overflow_diverts_before_the_next_statement_runs() {
    let (runtime, _) = run_main(
        "fn main() -> Int {\n  \
           var x = 9223372036854775807 + 1\n  \
           var y = 1 / 0\n  \
           x + y\n\
         }\n",
    );
    assert_eq!(
        runtime.fault(),
        praxis_runtime::FaultKind::IntOverflow,
        "the overflow must divert before the division runs; DivByZero here \
         means the check observed the raise too late"
    );
}

/// The same property for a fault raised inside a *wrapper* rather than by
/// inline arithmetic — the non-arithmetic path through the inline check.
///
/// `praxis_vec_get` is `Effect::Faults`: it stores `IndexOutOfBounds` and hands
/// back the Unit sentinel. The inline `CheckFault` that follows is the only
/// thing that stops the sentinel reaching the division below, and stops the
/// division overwriting the kind.
#[test]
fn a_fault_in_a_wrapper_is_observed_by_the_inline_check() {
    let (runtime, _) = run_main(
        "fn main() -> Int {\n  \
           var v = Vec()\n  \
           var x = v.get(0)\n  \
           var y = 1 / 0\n  \
           x + y\n\
         }\n",
    );
    assert_eq!(
        runtime.fault(),
        praxis_runtime::FaultKind::IndexOutOfBounds,
        "the wrapper's fault must divert at the check that follows it"
    );
}

/// `i64::MIN / -1` and `i64::MIN % -1` are the one overflowing signed division.
/// Cranelift's `sdiv`/`srem` *trap* on them — a process abort, not a Praxis
/// fault — so the lowering must keep those operands away from the instruction
/// and report `IntOverflow`, matching `praxis_int_div` / `praxis_int_rem`.
#[test]
fn int_min_divided_by_minus_one_overflows_rather_than_trapping() {
    let (runtime, _) = run_main("fn main() -> Int { (0 - 9223372036854775807 - 1) / (0 - 1) }");
    assert_eq!(runtime.fault(), praxis_runtime::FaultKind::IntOverflow);

    let (runtime, _) = run_main("fn main() -> Int { (0 - 9223372036854775807 - 1) % (0 - 1) }");
    assert_eq!(runtime.fault(), praxis_runtime::FaultKind::IntOverflow);
}

/// Arithmetic that cannot overflow must not fault, including the boundary
/// values the overflow predicates are written around.
#[test]
fn arithmetic_at_the_boundary_does_not_fault_spuriously() {
    for (src, want) in [
        ("fn main() -> Int { 9223372036854775807 - 1 }", i64::MAX - 1),
        ("fn main() -> Int { 0 - 9223372036854775807 }", -i64::MAX),
        ("fn main() -> Int { 0 - 7 / 2 }", -3),
        ("fn main() -> Int { 0 - 7 % 2 }", -1),
        (
            "fn main() -> Int { 4294967296 * 2147483647 }",
            4294967296i64 * 2147483647,
        ),
    ] {
        let (runtime, result) = run_main(src);
        assert!(
            !runtime.has_pending_fault(),
            "{src} faulted: {:?}",
            runtime.fault()
        );
        assert_eq!(result.as_int(), want, "{src}");
    }
}

/// The runtime symbol names the MIR for `src` emits, for asserting that a
/// lowering does *not* reach for a wrapper.
fn runtime_symbols_emitted_for(src: &str) -> std::collections::BTreeSet<&'static str> {
    let map = SourceMap::new();
    let file = map.intern("adversarial_audit.px", src);
    let parsed = parse(file, src);
    let mut analysis = analyze_root(file, &parsed.tree);
    let root = praxis_ast::SourceFile::cast(parsed.tree.clone()).expect("source root");
    let module = lower(file, &root, &mut analysis);
    let module = monomorphize(module, &analysis.names, &mut analysis.db);
    let mut funcs = lower_module(&module, &mut analysis.db);
    for func in &mut funcs {
        annotate(func);
        // Every host verifies after annotating; the tests are a host too, and
        // are the only one that runs the whole corpus (MIR-10).
        if let Err(errs) = praxis_mir::verify(func) {
            panic!("{}", praxis_mir::verify::report(&errs));
        }
    }
    funcs
        .iter()
        .flat_map(|f| f.blocks.iter())
        .flat_map(|b| b.insts.iter())
        .filter_map(|inst| match inst {
            praxis_mir::Inst::Call {
                callee: praxis_mir::CallTarget::Runtime(sym),
                ..
            } => Some(sym.name()),
            _ => None,
        })
        .collect()
}

/// RT-12, end to end. Each JIT generation interns its own schemas, so two
/// compiles of one program produce two `RecordSchema` allocations for one
/// record type. Equality compared those *addresses*, so a `Point { x: 1, y: 2 }`
/// from one compile was not equal to the identical value from the next — which
/// is what the debugger hits every time it evaluates `p` in its own module, and
/// why it works around the problem by sharing one evaluation generation.
///
/// Both records live on one heap here, so this is the comparison a program
/// could actually perform.
#[test]
fn records_from_two_generations_are_equal_when_their_type_is() {
    const NOMINAL: &str =
        "struct Point { x: Int, y: Int }\nfn main() -> Point { Point { x: 1, y: 2 } }\n";

    let (jit_a, ids_a) = compile(NOMINAL);
    let (jit_b, ids_b) = compile(NOMINAL);
    let mut runtime = Runtime::new();
    let mut context = runtime.context();
    context.input_source = runtime.alloc_text("");
    type ZeroArgMain = unsafe extern "C" fn(*mut RuntimeContext) -> GcRef;

    let entry_a: ZeroArgMain =
        unsafe { std::mem::transmute(jit_a.entry(*ids_a.get("main").expect("main"))) };
    let entry_b: ZeroArgMain =
        unsafe { std::mem::transmute(jit_b.entry(*ids_b.get("main").expect("main"))) };
    let a = unsafe { entry_a(&mut context as *mut RuntimeContext) };
    let b = unsafe { entry_b(&mut context as *mut RuntimeContext) };
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());

    // The two schemas really are distinct allocations, or this proves nothing.
    let schema_of = |r: GcRef| unsafe { (*r.payload::<praxis_runtime::RecordPayload>()).schema };
    assert!(
        !std::ptr::eq(schema_of(a), schema_of(b)),
        "two generations must have interned two schemas"
    );

    let equal =
        unsafe { praxis_runtime::abi::praxis_struct_eq(&mut context as *mut RuntimeContext, a, b) };
    assert_eq!(equal, 1, "one record type, two generations, one value");

    drop(jit_a);
    drop(jit_b);
}

/// The comparison lowerings a program emits, as a set of tags: which scalar
/// widths were extracted, and which of the four compare instructions ran.
///
/// P0-12 is a lowering choice, so asserting on the choice is what keeps it
/// fixed — a later refactor that reintroduces "extract eight bytes and compare"
/// for a `Text` or a `Char` fails here rather than at whatever the payload
/// happened to hold.
fn comparison_shapes_for(src: &str) -> std::collections::BTreeSet<String> {
    let map = SourceMap::new();
    let file = map.intern("adversarial_audit.px", src);
    let parsed = parse(file, src);
    let mut analysis = analyze_root(file, &parsed.tree);
    let root = praxis_ast::SourceFile::cast(parsed.tree.clone()).expect("source root");
    let module = lower(file, &root, &mut analysis);
    let module = monomorphize(module, &analysis.names, &mut analysis.db);
    let mut funcs = lower_module(&module, &mut analysis.db);
    for func in &mut funcs {
        annotate(func);
        if let Err(errs) = praxis_mir::verify(func) {
            panic!("{}", praxis_mir::verify::report(&errs));
        }
    }
    funcs
        .iter()
        .flat_map(|f| f.blocks.iter())
        .flat_map(|b| b.insts.iter())
        .filter_map(|inst| match inst {
            praxis_mir::Inst::ExtractScalar { scalar, .. } => Some(format!("extract:{scalar:?}")),
            praxis_mir::Inst::ValueCmp { .. } => Some("value_cmp".to_string()),
            praxis_mir::Inst::StructEq { .. } => Some("struct_eq".to_string()),
            praxis_mir::Inst::IntCmp { .. } => Some("int_cmp".to_string()),
            praxis_mir::Inst::FloatCmp { .. } => Some("float_cmp".to_string()),
            _ => None,
        })
        .collect()
}

/// P0-12. A `Text` comparison — of either kind — goes through the descriptor.
/// It must never extract a scalar from a `Text`: the payload is an enum of a
/// `Box<str>` and a `(GcRef, usize, usize)` slice, so an eight-byte load reads
/// the discriminant or a pointer.
#[test]
fn text_comparison_never_extracts_a_scalar_from_the_payload() {
    let ordering = comparison_shapes_for("fn main() -> Bool {\n  \"a\" < \"b\"\n}\n");
    assert!(
        ordering.contains("value_cmp"),
        "Text ordering dispatches to the descriptor's compare: {ordering:?}"
    );
    assert!(
        !ordering.iter().any(|s| s.starts_with("extract:")),
        "nothing is extracted from a Text payload: {ordering:?}"
    );

    let equality = comparison_shapes_for("fn main() -> Bool {\n  \"a\" == \"b\"\n}\n");
    assert!(
        equality.contains("struct_eq"),
        "Text equality dispatches to the descriptor's equals: {equality:?}"
    );
    assert!(
        !equality.iter().any(|s| s.starts_with("extract:")),
        "nothing is extracted from a Text payload: {equality:?}"
    );
}

/// P0-12. A `Char` payload is four bytes and a `Bool` one; both were extracted
/// as `Int`, an eight-byte load from a smaller, differently-aligned payload.
///
/// The `ScalarKind` this pins carries more weight since ADR-102: it no longer
/// only selects which wrapper is called, it selects the **inline** load's
/// descriptor and width. A defect of this class would now be an out-of-bounds
/// read in generated code rather than one inside a callee.
/// `a_bool_extract_reads_one_byte_and_a_char_four` (praxis-codegen-cranelift's
/// `lower.rs`) is the other half — this pins the kind, that pins the
/// instruction the kind selects.
#[test]
fn small_scalars_are_extracted_at_their_own_width() {
    // `grid(char)` was the only source of `Char` values when this was written.
    // It is not since ADR-086: `Text.get`/`t[i]` answer a `Char` too, so
    // `"#"[0]` names one. The language still has no char literal (D19). The
    // property under test is unaffected either way — this reads a grid because
    // that is what it read when P0-12 was measured.
    let chars = comparison_shapes_for(
        "fn main() -> Bool {\n  var g = read grid(char)\n  g.get(0, 0) < g.get(1, 0)\n}\n",
    );
    assert!(
        !chars.contains("extract:Int"),
        "a Char is never read as an eight-byte Int: {chars:?}"
    );
    assert!(
        chars.contains("value_cmp") || chars.contains("extract:Char"),
        "a Char comparison reads the payload at its own width, either natively \
         or through the descriptor: {chars:?}"
    );

    let bools = comparison_shapes_for(
        "fn main() -> Bool {\n  var a = 1 == 1\n  var b = 2 == 2\n  a == b\n}\n",
    );
    // Since ADR-121 the strongest form of this is what actually happens: `a` and
    // `b` are promoted to `Scalar(Bool)` slots, so the comparison is an
    // `IntCmp` over two raw words and there is **no** payload read to get the
    // width of. The property under test is "a `Bool` is never read as an
    // eight-byte `Int`", and no read at all satisfies it more completely than a
    // one-byte read does — so the assertion is the absence of the defect, not
    // the presence of the instruction that used to avoid it.
    assert!(
        !bools.contains("extract:Int"),
        "a Bool is never read as an eight-byte Int: {bools:?}"
    );
    assert!(
        bools.contains("extract:Bool") || !bools.iter().any(|s| s.starts_with("extract:")),
        "a Bool comparison reads the payload at its own width, or reads no \
         payload because promotion left no object to read: {bools:?}"
    );
}

/// MIR-01: a shadow slot whose local has died must be nulled, or the collector
/// keeps reading it and the object never becomes garbage.
///
/// The two programs differ only in whether `xs` is read again after the loop
/// that fills it. `xs` and its three thousand elements are reachable from the
/// frame in exactly one of them — so if the slot is never cleared, both keep
/// them, and the heaps come out the same size. Comparing the two rather than
/// asserting an absolute count keeps the test honest about collection timing:
/// whatever residue the arithmetic loop leaves, both programs leave the same.
///
/// **The elements are offset past the interned small-`Int` range on purpose**
/// (`praxis_runtime::small_int`). An interned `Int` is an immortal that no
/// collection reclaims, so a `Vec` of `0..3000` holds only ~2000 reclaimable
/// objects and the margin below stops being a margin. The offset keeps all
/// three thousand elements real allocations, which is what makes "freed almost
/// nothing" a statement about the shadow slot rather than about interning.
///
/// **The second loop has to allocate, and saying so is ADR-121's doing.** Its
/// job is to run the collector while `main`'s frame is live — a slot that is
/// never scanned cannot be observed to have been nulled — and it used to do
/// that as a side effect of `sum = sum + j` boxing a fresh out-of-range `Int`
/// per iteration. Promotion turns both `sum` and `j` into `Scalar` slots, so
/// that loop became pure register arithmetic that allocates nothing, no
/// collection ran in *either* program, and the two heaps came out at 3003 and
/// 3004 live objects — a difference of one, and the test read it as `xs` still
/// being rooted. The pressure is now a `Vec` per iteration that dies at the end
/// of it, which is allocation no scalar optimization can remove.
#[test]
fn a_dead_local_stops_being_reachable_from_its_frame() {
    const FILL_AND_LOOP: &str = "\
fn main() -> Int {
  var xs = Vec()
  var i = 0
  while i < 3000 {
    xs.push(i + 2000)
    i = i + 1
  }
  var sum = 0
  var j = 0
  while j < 20000 {
    var garbage = Vec()
    garbage.push(j + 2000)
    sum = sum + garbage.len()
    j = j + 1
  }
  sum";

    // `xs` is dead the moment the filling loop ends.
    let (dead, _) = run_main(&format!("{FILL_AND_LOOP}\n}}\n"));
    // Identical, except the tail reads `xs` — so it is live throughout.
    let (kept, _) = run_main(&format!("{FILL_AND_LOOP} + xs.len()\n}}\n"));

    let dead_live = dead.heap().stats().live_count;
    let kept_live = kept.heap().stats().live_count;
    assert!(
        dead_live + 2_500 < kept_live,
        "dropping `xs` freed almost nothing, so its slot is still rooting it: \
         dead = {dead_live}, kept alive = {kept_live}"
    );
}

// ---------------------------------------------------------------------------
// ADR-127: the pipeline over every iterable, end to end.
// ---------------------------------------------------------------------------

/// **Decision 2, as behaviour.** A pipeline over a `Set` and a `MinHeap`
/// produces the right answer, which is the half the instruction-count test in
/// `praxis_mir::build` cannot state.
///
/// The failure this rules out is not "the wrong number": reading a `Set`'s
/// payload through `praxis_vec_get` hung or killed the process, and a
/// `MinHeap`'s was a silently wrong answer — the two failure modes `IterPlan`
/// exists to prevent, and the reason the pipeline had to stop opening its source
/// with a hardcoded `Vec` accessor pair.
#[test]
fn a_pipeline_over_a_snapshotted_collection_answers_from_every_member() {
    let (runtime, result) = run_main(
        "fn main() -> Int {\n  var s = Set()\n  s.insert(3)\n  s.insert(1)\n  \
         s.insert(2)\n  s.map(|x| x * 2).sum()\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 12, "every member, doubled");

    // A heap's backing array is heap-ordered only at its root, so a direct
    // index walk reads the members in an order that is not the heap's — and
    // reads whatever the array holds past its logical end.
    let (runtime, result) = run_main(
        "fn main() -> Int {\n  var h = MinHeap()\n  h.push(5)\n  h.push(1)\n  \
         h.push(9)\n  h.map(|x| x + 1).sum()\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 18);

    // A `Map` yields pairs from two aligned snapshots, joined per step.
    let (runtime, result) = run_main(
        "fn main() -> Int {\n  var m = Map()\n  m.insert(1, 10)\n  m.insert(2, 20)\n  \
         m.map(|kv| kv.0 * kv.1).sum()\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 50, "the halves stayed aligned");

    // A `Text` walks through its own accessors, so the item is the `Char`
    // `t[i]` answers rather than a byte.
    let (runtime, result) =
        run_main("fn main() -> Int {\n  \"héllo\".count(|c| c == \"l\"[0])\n}\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 2);
}

/// **Decision 3.** A barrier over a non-`Vec` receiver answers over every
/// member, in the deterministic order the snapshot fixes.
///
/// `praxis_vec_sorted` reads a `VecPayload`. Handing it a `Set` or a `Deque`
/// directly is a wrong-type read; handing it the materialization is the whole of
/// the decision.
#[test]
fn a_barrier_over_a_non_vec_receiver_sees_every_member() {
    let (runtime, result) = run_main(
        "fn main() -> Int {\n  var s = Set()\n  s.insert(3)\n  s.insert(1)\n  \
         s.insert(2)\n  s.sorted()[0]\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 1);

    // A `Deque` has no snapshot symbol at all — it indexes itself but is not a
    // `Vec` — so its materialization is the walk `to_vec` fuses.
    let (runtime, result) = run_main(
        "fn main() -> Int {\n  var d = Deque()\n  d.push_back(3)\n  d.push_front(7)\n  \
         d.sorted()[1]\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 7);

    // …and a `Range`, whose members exist only as an arithmetic rule.
    let (runtime, result) = run_main("fn main() -> Int {\n  (0..4).unique().len()\n}\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 4);
}

/// **Decision 3, for the two barriers added by ADR-144 and ADR-145.** Each reads
/// its receiver through that receiver's own accessor and never through
/// `praxis_vec_get` on a foreign payload.
///
/// The receivers are chosen so a wrong-type read cannot be mistaken for a right
/// answer: a `Range`'s members exist only as an arithmetic rule and a `Deque`
/// has no snapshot symbol at all, so if the materializing walk were skipped the
/// wrapper would be reading a `RangeVal` or a `DequePayload` as a `VecPayload`.
#[test]
fn reversal_and_join_read_every_receiver_through_its_own_accessor() {
    // A `Range`: `(0..4).reversed()[0]` is 3 only if all four members were
    // produced by the range's own walk.
    let (runtime, result) = run_main("fn main() -> Int {\n  (0..4).reversed()[0]\n}\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 3);

    // A `Text`, whose item is the `Char` `t[i]` answers.
    let (runtime, result) = run_main("fn main() -> Char {\n  \"héllo\".reversed()[0]\n}\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_char(), 'o');

    // A `Deque`, which indexes itself but is not a `Vec`.
    let (runtime, result) = run_main(
        "fn main() -> Int {\n  var d = Deque()\n  d.push_back(3)\n  d.push_front(7)\n  \
         d.reversed()[0]\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 3);

    // `join` over a `Map`'s keys: the source is a `Vec` the wrapper did not
    // build, and the separator is the second argument rather than the receiver.
    let (runtime, result) = run_main(
        "fn main() -> Text {\n  var m = Map()\n  m[\"b\"] = 1\n  m[\"a\"] = 2\n  \
         m.keys().join(\"|\")\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_text(), "a|b", "keys walk in the key's own order");

    // And `to_text` over a `Vec[Char]` the language built for itself, rather
    // than one that came off a `Grid`.
    let (runtime, result) = run_main("fn main() -> Text {\n  \"abc\".reversed().to_text()\n}\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_text(), "cba");
}

/// **Decision 4.** Each conversion builds the collection it names, from every
/// member of whatever it started on.
#[test]
fn a_conversion_builds_the_collection_it_names() {
    for (expr, want) in [
        // Out of a `Set`, which has no member accessor in the language at all.
        ("s.to_vec().sum()", 6),
        ("s.to_deque().len()", 3),
        ("s.to_min_heap().pop()", 1),
        ("s.to_max_heap().pop()", 3),
        ("s.to_bitset().len()", 3),
        ("s.to_set().len()", 3),
        // A fused sink: no intermediate `Vec`, and the duplicates the map
        // introduces are dropped by the `Set` rather than by a second pass.
        ("s.map(|x| x % 2).to_set().len()", 2),
    ] {
        let src = format!(
            "fn main() -> Int {{\n  var s = Set()\n  s.insert(3)\n  s.insert(1)\n  \
             s.insert(2)\n  {expr}\n}}\n"
        );
        let (runtime, result) = run_main(&src);
        assert!(
            !runtime.has_pending_fault(),
            "{expr} faulted: {:?}",
            runtime.fault()
        );
        assert_eq!(result.as_int(), want, "{expr}");
    }

    // The two keyed conversions, which are the routes *back in*. `to_map` takes
    // the item pair apart with the same `praxis_tuple_get` a `p.0` emits.
    let (runtime, result) = run_main(
        "fn main() -> Int {\n  var m = Map()\n  m.insert(1, 10)\n  m.insert(2, 20)\n  \
         m.map(|kv| (kv.0, kv.1 * 2)).to_map()[2]\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 40);

    // `to_counter` *assigns* the count each pair carries, where `frequencies`
    // counts occurrences — the two directions of one type change.
    let (runtime, result) = run_main(
        "fn main() -> Int {\n  var v = Vec()\n  v.push(\"a\")\n  v.push(\"b\")\n  \
         v.push(\"a\")\n  v.frequencies().to_vec().to_counter()[\"a\"]\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 2);

    // `to_vec` on a `Vec` answers **the same reference**, not a copy (ADR-126
    // decision 2's second half). A copy would leave the original at length 1.
    let (runtime, result) = run_main(
        "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  var w = v.to_vec()\n  \
         w.push(2)\n  v.len()\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 2, "`to_vec` on a `Vec` is the identity");

    // `to_bitset` still faults on a member no `BitIndex` can hold — a *value*
    // question, which the row's `Int` item type cannot answer.
    let (runtime, _result) = run_main(
        "fn main() -> Int {\n  var v = Vec()\n  v.push(0 - 1)\n  v.to_bitset().len()\n}\n",
    );
    assert!(
        runtime.has_pending_fault(),
        "a negative member is `praxis_bitset_insert`'s fault to raise"
    );
}

/// **Decision 5.** `sorted_by_key` orders by the key the closure extracts, and
/// the sort is stable — so the answer is a function of the input alone.
///
/// The whole point is a pipeline whose item is a **pair**: no composite is
/// orderable in the *source language* (ADR-045; the container order a `Map` now
/// walks its keys in is a different question, ADR-138), so `sorted` is
/// unavailable the moment the source is a `Map` or a `Counter`, and "the most
/// common value" had no spelling.
#[test]
fn sorted_by_key_orders_a_pair_by_the_key_it_carries() {
    let (runtime, result) = run_main(
        "fn main() -> Text {\n  var v = Vec()\n  v.push(\"the\")\n  v.push(\"cat\")\n  \
         v.push(\"the\")\n  v.push(\"cat\")\n  v.push(\"the\")\n  \
         v.frequencies().to_vec().sorted_by_key(|p| 0 - p.1)[0].0\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_text(), "the");

    // Stability: equal keys keep their input order, so the answer does not
    // depend on the sort's internals.
    let (runtime, result) = run_main(
        "fn main() -> Int {\n  var v = Vec()\n  v.push(10)\n  v.push(21)\n  v.push(30)\n  \
         v.sorted_by_key(|x| x % 2)[0]\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 10);

    // A `Text` key, which is the case a payload-bytes comparison gets wrong: a
    // `Text` is a pointer-and-length structure, so ordering one by its first
    // eight bytes compares *addresses* (P0-12). The key goes through the
    // descriptor's `compare`, as `sorted` does.
    let (runtime, result) = run_main(
        "fn main() -> Int {\n  var v = Vec()\n  v.push(3)\n  v.push(1)\n  v.push(2)\n  \
         v.sorted_by_key(|x| \"abc\"[x - 1])[0]\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 1);

    // The receiver is generic like every other barrier's, so a `Set` sorts too.
    let (runtime, result) = run_main(
        "fn main() -> Int {\n  var s = Set()\n  s.insert(30)\n  s.insert(1)\n  \
         s.insert(200)\n  s.sorted_by_key(|x| 0 - x)[0]\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 200);
}
