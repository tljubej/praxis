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
        run_main("fn main() {\n  let v = Vec()\n  v.push(10)\n  v.push(20)\n  v.enumerate()\n}\n");
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
        run_main("fn main() {\n  let v = Vec()\n  v.push(10)\n  v.enumerate()\n}\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(
        tuple_element_descriptor_slots(result.as_vec()[0]),
        vec![Some(int), Some(int)],
        "enumerate's pair is (Int, T), and both halves are named"
    );

    // `zip` pairs two *different* element types, so a schema that echoed the
    // receiver's element type for both slots would fail here and not above.
    let (runtime, result) = run_main(
        "fn main() {\n  let a = Vec()\n  a.push(1)\n  let b = Vec()\n  b.push(\"s\")\n  a.zip(b)\n}\n",
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
        run_main("fn main() -> Int {\n  let v = Vec()\n  v.enumerate().count()\n}\n");
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
        "fn main() {\n  let a = Vec()\n  a.push(1)\n  a.push(2)\n  let b = Vec()\n  b.push(10)\n  b.push(20)\n  a.zip(b)\n}\n",
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
        "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.push(5)\n  v.filter(|x| x % 2 == 0).take(2).sum()\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 6, "take(2) applies after filter");
}

#[test]
fn skip_after_filter_counts_filtered_elements_not_source_indices() {
    let (runtime, result) = run_main(
        "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.push(6)\n  v.filter(|x| x % 2 == 0).skip(1).sum()\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 10, "skip(1) drops the first filtered item");
}

#[test]
fn zip_after_filter_uses_dense_filtered_positions() {
    let (runtime, result) = run_main(
        "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  let rhs = Vec()\n  rhs.push(10)\n  rhs.push(20)\n  v.filter(|x| x % 2 == 0).zip(rhs).count()\n}\n",
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
        "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.filter(|x| x % 2 == 0).position(|x| x == 4)\n}\n",
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
        "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.flat_map(|x| {\n    let a = Vec()\n    a.push(x)\n    a\n  }).flat_map(|x| {\n    let b = Vec()\n    b.push(x)\n    b\n  }).count()\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn take_after_flat_map_counts_the_global_flattened_stream() {
    let (runtime, result) = run_main(
        "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.flat_map(|x| {\n    let inner = Vec()\n    inner.push(x)\n    inner\n  }).take(1).count()\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn position_after_flat_map_uses_the_global_flattened_index() {
    let (runtime, result) = run_main(
        "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.flat_map(|x| {\n    let inner = Vec()\n    inner.push(x)\n    inner\n  }).position(|x| x == 2)\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn any_after_flat_map_short_circuits_the_whole_pipeline() {
    let (runtime, result) = run_main(
        "fn main() -> Bool {\n  let v = Vec()\n  v.push(1)\n  v.push(0)\n  v.flat_map(|x| {\n    let inner = Vec()\n    inner.push(x)\n    inner\n  }).any(|x| x == 1 || 10 / x > 0)\n}\n",
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
        "struct P { x: Int }\nfn main() -> Int {\n  let a = (P { x: 1 }, 0)\n  let b = (P { x: 2 }, 0)\n  if a == b { 1 } else { 0 }\n}\n",
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
        "fn main() -> Int {\n  let a = Vec()\n  a.push(0.0)\n  let b = Vec()\n  b.push(-0.0)\n  if a == b { 1 } else { 0 }\n}\n",
    );
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 1, "+0.0 and -0.0 are equal Floats");
}

#[test]
fn empty_vec_float_has_the_float_element_descriptor_before_any_push() {
    let (runtime, result) =
        run_main("fn main() -> Vec[Float] {\n  let values: Vec[Float] = Vec()\n  values\n}\n");
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
        "fn main() -> Int {{ let x = Seed{TARGET} {{ value: 7 }}; x.value }}"
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
        "fn main() -> Int {{\n  let a = Probe{TARGET} {{ value: \"left\" }}\n  let b = Probe{TARGET} {{ value: \"right\" }}\n  if a == b {{ 1 }} else {{ 0 }}\n}}"
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
    let src = "fn main() -> Int {\n  let key = Vec()\n  key.push(1)\n  let m = Map()\n  m.insert(key, 42)\n  key.push(2)\n  if m.contains(key) { 1 } else { 0 }\n}\n";
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
    // threshold. This loop creates at least ten registered Int allocations per
    // iteration (~100k total). With no sweep, live_count remains >=100k; after
    // automatic GC it is well below that even though the arithmetic result is
    // unchanged.
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
/// to be far below what was allocated, or no sweep ran and the test proves
/// nothing.
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
               let ms = read scan(choice(\n    \
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
    let src = "fn main() -> Text {\n  let groups = read sections(lines(word))\n  groups.get(1).get(0)\n}\n";
    let (runtime, result) = run_main_with_input(src, "alpha\n\nbeta\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_text(), "beta");
}

#[test]
fn lines_require_each_child_parser_to_consume_the_whole_line() {
    // `int` may consume the `12` prefix, but the trailing `junk` makes the line
    // invalid under §7.5 full-consumption semantics.
    let (runtime, _raw, _unit) = run_main_raw_with_input(
        "fn main() -> Int {\n  let values = read lines(int)\n  values.len()\n}\n",
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
    let src = "fn main() -> Text {\n  let values = read lines(rest)\n  values.get(1)\n}\n";
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
    let src = "fn main() -> Text {\n  let parsed = read `pre{body:text}post`\n  parsed.body\n}\n";
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
    let src = "fn main() -> Int {\n  let g = read grid(char)\n  g.width() * 10 + g.height()\n}\n";
    let (runtime, result) = run_main_with_input(src, "a b\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 31, "\"a b\" is three columns, not two");

    // And it is readable at the column it occupies.
    let src = "fn main() -> Char {\n  let g = read grid(char)\n  g.get(1, 0)\n}\n";
    let (runtime, result) = run_main_with_input(src, "a b\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_char(), ' ');

    // Trailing and leading spaces are cells too, for the same reason.
    let src = "fn main() -> Int {\n  let g = read grid(char)\n  g.width() * 10 + g.height()\n}\n";
    let (runtime, result) = run_main_with_input(src, "ab \n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 31);
    let (runtime, result) = run_main_with_input(src, " a\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 21);

    // A genuinely ragged grid is rejected again, which is the point: §7.5 says
    // every row has the same cell count, and this input does not.
    let (runtime, _raw, _unit) = run_main_raw_with_input(
        "fn main() -> Int {\n  let g = read grid(char)\n  g.width()\n}\n",
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
        "fn main() -> Int {\n  let g = read grid(int)\n  g.width() * 100 + g.height() * 10 + g[1, 1]\n}\n";
    let (runtime, result) = run_main_with_input(src, "12 34 \n56 78 \n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 2 * 100 + 2 * 10 + 78);

    // The same file through `matrix`, which never had the defect: the two
    // constructors agree now, and that agreement is the assertion.
    let src = "fn main() -> Int {\n  let g = read matrix(int)\n  g.width() * 100 + g.height() * 10 + g[1, 1]\n}\n";
    let (runtime, result) = run_main_with_input(src, "12 34 \n56 78 \n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 2 * 100 + 2 * 10 + 78);
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
        "fn main() -> Text {\n  let r = read lines(`{name:text} {v:int}`)\n  r.get(1).name\n}\n";
    let (runtime, result) = run_main_with_input(src, "foo 3\nbar 12\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_text(), "bar");

    // `\s+`.
    let src = "fn main() -> Int {\n  let r = read lines(`{name:text}\\s+{v:int}`)\n  var t = 0\n  for x in r { t = t + x.v }\n  t\n}\n";
    let (runtime, result) = run_main_with_input(src, "foo 3\nbar 12\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 15);

    // `\n`, which bounds a capture across a line ending.
    let src = "fn main() -> Int {\n  let r = read `{name:text}\\n{v:int}`\n  r.v\n}\n";
    let (runtime, result) = run_main_with_input(src, "foo\n3\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 3);

    // `\s*` still constrains nothing, so it is still skipped: the scan looks
    // past it for something that does. Nothing does here, so the capture is
    // unbounded and `text` takes the line — which is the documented answer for
    // a template that asks for zero-or-more.
    let src = "fn main() -> Text {\n  let r = read lines(`{name:text}\\s*`)\n  r.get(0).name\n}\n";
    let (runtime, result) = run_main_with_input(src, "foo 3\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_text(), "foo 3");
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
    let src = "fn main() -> Text {\n  let r = read `{w:word}-to-{x:word} map:`\n  \
               r.w\n}\n";
    let (runtime, result) = run_main_with_input(src, "seed-to-soil map:");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(
        result.as_text(),
        "seed",
        "the following literal is the bound; `-` is not a word delimiter"
    );

    let src = "fn main() -> Text {\n  let r = read `{w:word}-to-{x:word} map:`\n  \
               r.x\n}\n";
    let (runtime, result) = run_main_with_input(src, "seed-to-soil map:");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_text(), "soil");

    // Unbounded, `-` is an ordinary word character and must stay one.
    let src = "fn main() -> Text {\n  let v = read ws(word)\n  v.get(0)\n}\n";
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
        "fn id(x) { x }\nfn main() -> Int {\n  let word = id(\"four\")\n  id(38) + word.len()\n}\n",
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
        "fn main() {\n  let xs = Vec()\n  let nothing = xs.push(1)\n  let pair = (nothing, 7)\n  pair\n}\n",
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
        "fn main() {\n  let g = read grid(char)\n  g.positions()\n}\n",
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
        "fn main() {\n  let g = read matrix(word)\n  g.row(0)\n}\n",
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
               \x20 let g = read matrix(int)\n\
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
               \x20 let g = read matrix(int)\n\
               \x20 match g.find(4) {\n    Some(p) => 1,\n    None => 0,\n  }\n\
               }\n";
    let (runtime, result) = run_main_with_input(src, "1 2\n3 4\n");
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 1, "a hit is `Some((x, y))`");

    // And the `Option` really is an enum value, not a tuple that happens to
    // answer: the descriptor is what `format`/`equals`/`hash` dispatch through.
    let (runtime, result) = run_main_with_input(
        "fn main() {\n  let g = read matrix(int)\n  g.find(99)\n}\n",
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
        "{unwrap}fn main() -> Int {{\n  let m = Map()\n  m.insert(1, 10)\n  unwrap(m.get(2))\n}}\n"
    ));
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), -1);
    let (runtime, result) = run_main(&format!(
        "{unwrap}fn main() -> Int {{\n  let m = Map()\n  m.insert(1, 10)\n  unwrap(m.get(1))\n}}\n"
    ));
    assert!(!runtime.has_pending_fault(), "fault: {:?}", runtime.fault());
    assert_eq!(result.as_int(), 10);

    let (runtime, result) =
        run_main("fn main() {\n  let m = Map()\n  m.insert(1, 10)\n  m.get(2)\n}\n");
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
            "fn main() -> Int {\n  let v = Vec()\n  v.reduce(|a, x| a + x)\n}\n",
        ),
        (
            "min_by",
            "fn main() -> Int {\n  let v = Vec()\n  v.min_by(|a, b| a < b)\n}\n",
        ),
        (
            "max_by",
            "fn main() -> Int {\n  let v = Vec()\n  v.max_by(|a, b| a < b)\n}\n",
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
/// The non-ASCII cell is unparseable for a reason that has nothing to do with
/// ordering — the `grid(char)` cell parser works in bytes, so `β` is an
/// "expected char" mismatch, and `read grid(char)` is the only source of `Char`
/// values in the language (`Text.get` returns the scalar value as an `Int`).
/// That is an input-parser defect, S19/S20's territory alongside IPR-06's
/// `grid(int)` granularity, and leaving it here would make an ordering test go
/// red until a parser fix lands.
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
        "fn main() -> Bool {\n  let g = read grid(char)\n  g.get(0, 0) < g.get(1, 0)\n}\n",
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
#[test]
fn small_scalars_are_extracted_at_their_own_width() {
    // `grid(char)` is the only source of `Char` values today — `Text.get`
    // returns the scalar value as an `Int`, and the language has no char
    // literal.
    let chars = comparison_shapes_for(
        "fn main() -> Bool {\n  let g = read grid(char)\n  g.get(0, 0) < g.get(1, 0)\n}\n",
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
        "fn main() -> Bool {\n  let a = 1 == 1\n  let b = 2 == 2\n  a == b\n}\n",
    );
    assert!(
        bools.contains("extract:Bool"),
        "a Bool comparison loads a Bool: {bools:?}"
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
#[test]
fn a_dead_local_stops_being_reachable_from_its_frame() {
    const FILL_AND_LOOP: &str = "\
fn main() -> Int {
  let xs = Vec()
  var i = 0
  while i < 3000 {
    xs.push(i)
    i = i + 1
  }
  var sum = 0
  var j = 0
  while j < 20000 {
    sum = sum + j
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
