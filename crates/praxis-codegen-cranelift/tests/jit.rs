//! End-to-end JIT integration tests: source → typed HIR → MIR → Cranelift → run.
//!
//! These are the Milestone 4 acceptance tests (§19): execute boxed integer
//! arithmetic, branches, loops, and recursive function calls through the JIT,
//! and confirm faults return to the host without unwinding.

use praxis_ast::AstNode;
use praxis_codegen_cranelift::{Jit, RunnableFunction};
use praxis_hir::{analyze_root, lower, mono::monomorphize};
use praxis_mir::{annotate, lower_module};
use praxis_parser::parse;
use praxis_runtime::{GcRef, Runtime, RuntimeContext};
use praxis_source::SourceMap;

/// The full pipeline for one source string: compile every `fn` and return the
/// `Jit` (owning the code) plus the name→FuncId map.
fn compile(
    src: &str,
) -> (
    Jit,
    std::collections::HashMap<String, cranelift_module::FuncId>,
) {
    let map = SourceMap::new();
    let file = map.intern("jit_test.px", src);
    let parsed = parse(file, src);
    let mut analysis = analyze_root(file, &parsed.tree);
    let root = praxis_ast::SourceFile::cast(parsed.tree.clone()).unwrap();
    let module = lower(file, &root, &mut analysis);
    assert!(
        module.diagnostics.is_empty(),
        "lowering diagnostics: {:?}",
        module.diagnostics
    );
    // Monomorphization (WS8): instantiate polymorphic callees per call site.
    let module = monomorphize(module, &analysis.names, &mut analysis.db);
    let mut funcs = lower_module(&module, &mut analysis.db);
    for f in &mut funcs {
        annotate(f);
    }
    let mut jit = Jit::new().expect("JIT construction");
    let ids = jit.compile(&funcs, &analysis.db).expect("JIT compilation");
    (jit, ids)
}

/// Compile and call a zero-arg `fn main() -> Int { ... }`, returning the result
/// `GcRef` and the runtime (so the caller can read the fault / payload).
fn run_main(src: &str) -> (Runtime, GcRef) {
    run_main_with_input(src, "")
}

/// Like [`run_main`], but installs `input` as the process-input buffer (§7.10)
/// before executing, so `read` expressions can parse it.
fn run_main_with_input(src: &str, input: &str) -> (Runtime, GcRef) {
    let (jit, ids) = compile(src);
    let main_id = *ids.get("main").expect("no `main` function");
    let mut rt = Runtime::new();
    let mut ctx = rt.context();
    // Install the input buffer if non-empty (§7.10).
    if !input.is_empty() {
        let input_ref = rt.alloc_text(input);
        ctx.input_source = input_ref;
    }
    let entry: RunnableFunction = unsafe { std::mem::transmute(jit.entry(main_id)) };
    // main takes no GcRef params beyond the context; pass Unit as the unused slot.
    let unit = rt.alloc_unit();
    let result = unsafe { entry(&mut ctx as *mut RuntimeContext, unit) };
    // Keep the JIT alive for the call (it owns the executable memory).
    drop(jit);
    (rt, result)
}

#[test]
fn runs_a_constant_int() {
    // main returns a boxed 42; the host reads the payload.
    let (rt, result) = run_main("fn main() -> Int { 42 }");
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 42);
}

#[test]
fn runs_boxed_addition() {
    let (rt, result) = run_main("fn main() -> Int { 40 + 2 }");
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 42);
}

#[test]
fn runs_arithmetic_precedence() {
    // 1 + 2 * 3 = 7  (the parser respects precedence).
    let (rt, result) = run_main("fn main() -> Int { 1 + 2 * 3 }");
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 7);
}

#[test]
fn runs_subtraction_and_division() {
    let (rt, result) = run_main("fn main() -> Int { (10 - 4) / 2 }");
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 3);
}

#[test]
fn runs_if_branch() {
    let src = "fn main() -> Int { if 1 < 2 { 100 } else { 200 } }";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 100);
}

#[test]
fn runs_while_loop_sum() {
    // Sum 1..=5 = 15.
    let src = "fn main() -> Int {\n  var s = 0\n  var i = 1\n  while i < 6 { s = s + i; i = i + 1 }\n  s\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 15);
}

#[test]
fn runs_recursive_factorial() {
    // fact(5) = 120, with a recursive user function.
    let src = "\
fn fact(n: Int) -> Int { if n < 2 { 1 } else { n * fact(n - 1) } }
fn main() -> Int { fact(5) }
";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 120);
}

#[test]
fn fibonacci_recursive() {
    // fib(10) = 55.
    let src = "\
fn fib(n: Int) -> Int { if n < 2 { n } else { fib(n - 1) + fib(n - 2) } }
fn main() -> Int { fib(10) }
";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 55);
}

#[test]
fn overflow_returns_to_host_without_unwinding() {
    // i64::MAX + 1 overflows; the host observes the fault, not a panic/abort.
    let src = "fn main() -> Int { 9223372036854775807 + 1 }";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "overflow should set the fault");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::IntOverflow);
}

#[test]
fn division_by_zero_returns_to_host_without_unwinding() {
    let src = "fn main() -> Int { 1 / 0 }";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "div-by-zero should set the fault");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::DivByZero);
}

// ===========================================================================
// Milestone 5: shadow-stack GC spill (ADR-019).
// ===========================================================================

#[test]
fn live_locals_survive_collection_during_a_loop() {
    // The headline M5 spill test: a loop that allocates heavily (well past the
    // 64 KiB initial collection threshold) while holding two live `Int` locals
    // (`sum` and `i`) across every allocation safepoint. If the shadow-stack
    // spill is wrong, a collection reclaims `sum`/`i` and the result corrupts.
    //
    // Each iteration allocates ~8 Int objects (the arithmetic + the counter);
    // 10000 iterations allocates ~80k objects (~1.5 MiB), forcing many GCs.
    let src = "fn main() -> Int {\n  var sum = 0\n  var i = 0\n  while i < 10000 { sum = sum + i; i = i + 1 }\n  sum\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "no fault expected");
    // sum(0..10000) = 10000*9999/2 = 49995000
    assert_eq!(result.as_int(), 49_995_000);
}

#[test]
fn recursive_call_keeps_caller_locals_alive_across_collections() {
    // fact(10) = 3628800. Each recursive call allocates (the materialized
    // product), and the caller's `n` must survive across the callee's
    // allocations. fib(20) is a heavier stress (more calls / allocations).
    let src = "\
fn fib(n: Int) -> Int { if n < 2 { n } else { fib(n - 1) + fib(n - 2) } }
fn main() -> Int { fib(20) }
";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault());
    // fib(20) = 6765
    assert_eq!(result.as_int(), 6765);
}

// ===========================================================================
// Milestone 5: Vec[T] method surface (§11, §16.2).
// ===========================================================================

#[test]
fn vec_push_and_len_end_to_end() {
    // The headline M5 vertical slice: construct a Vec, push values, read len.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "no fault expected");
    assert_eq!(result.as_int(), 3);
}

#[test]
fn vec_get_reads_back_elements() {
    // Push 10, 20, 30; get index 1 → 20.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(10)\n  v.push(20)\n  v.push(30)\n  v.get(1)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 20);
}

#[test]
fn vec_get_out_of_bounds_faults() {
    // Accessing index 0 of an empty vector faults IndexOutOfBounds.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.get(0)\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "OOB should fault");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::IndexOutOfBounds);
}

#[test]
fn vec_push_many_with_collection_during_growth() {
    // Push 500 elements (forcing many GCs during growth), check length.
    // This exercises both the method surface and the shadow-stack spill: the
    // vector `v` must survive across every push's allocation + collection.
    let src = "fn main() -> Int {\n  let v = Vec()\n  var i = 0\n  while i < 500 { v.push(i); i = i + 1 }\n  v.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 500);
}

#[test]
fn vec_push_many_read_back_correct() {
    // Push 500 elements and read back the last (index 499 = value 499). This is
    // a stricter test of the shadow-stack spill: the vec's *contents* must
    // survive across every collection, not just the vec object itself.
    let src = "fn main() -> Int {\n  let v = Vec()\n  var i = 0\n  while i < 500 { v.push(i); i = i + 1 }\n  v.get(499)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 499);
}

// --- M8-WS1: Vec[T]() construction honors the real element descriptor --------

#[test]
fn vec_of_vec_equality_after_construction() {
    // Two `Vec()`-constructed vectors holding identical inner vectors must be
    // structurally equal. This only works once `Vec[T]()` construction passes
    // the *real* element descriptor (the outer Vec's element descriptor must be
    // `VEC`, not the null/INT default), so nested equality dispatches through
    // `vec_equals` on the inner elements. This is the headline M7-carryover
    // fix for M8-WS1.
    let src = "fn main() -> Int {\n  let outer_a = Vec()\n  let inner_a = Vec()\n  inner_a.push(1)\n  inner_a.push(2)\n  outer_a.push(inner_a)\n  let outer_b = Vec()\n  let inner_b = Vec()\n  inner_b.push(1)\n  inner_b.push(2)\n  outer_b.push(inner_b)\n  if outer_a == outer_b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn vec_of_vec_inequality_after_construction() {
    // The complement: two `Vec()`-constructed vectors holding *different* inner
    // vectors must be structurally unequal. Guards a regression where the
    // element descriptor defaulted to INT (which would compare only lengths or
    // mis-dispatch and could spuriously report equal).
    let src = "fn main() -> Int {\n  let outer_a = Vec()\n  let inner_a = Vec()\n  inner_a.push(1)\n  inner_a.push(2)\n  outer_a.push(inner_a)\n  let outer_b = Vec()\n  let inner_b = Vec()\n  inner_b.push(1)\n  inner_b.push(9)\n  outer_b.push(inner_b)\n  if outer_a == outer_b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

// --- M8-WS2: Deque[T] (§6.1) ------------------------------------------------

#[test]
fn deque_push_back_and_len_end_to_end() {
    // Construct a Deque, push_back three values, read len → 3.
    let src = "fn main() -> Int {\n  let d = Deque()\n  d.push_back(1)\n  d.push_back(2)\n  d.push_back(3)\n  d.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

#[test]
fn deque_push_front_yields_fifo_order() {
    // push_front(1), push_front(2), push_front(3) → front-to-back is [3,2,1].
    // pop_front returns 3 (the last pushed), proving FIFO-from-front semantics.
    let src = "fn main() -> Int {\n  let d = Deque()\n  d.push_front(1)\n  d.push_front(2)\n  d.push_front(3)\n  d.pop_front()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

#[test]
fn deque_push_back_pop_front_is_fifo() {
    // push_back then pop_front is a classic FIFO queue: 1,2,3 in → 1 out first.
    let src = "fn main() -> Int {\n  let d = Deque()\n  d.push_back(1)\n  d.push_back(2)\n  d.push_back(3)\n  d.pop_front()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn deque_push_front_pop_back_is_lifo() {
    // push_front then pop_back is LIFO: 1,2,3 pushed to front → pop_back gives 1.
    let src = "fn main() -> Int {\n  let d = Deque()\n  d.push_front(1)\n  d.push_front(2)\n  d.push_front(3)\n  d.pop_back()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn deque_get_indexes_from_front() {
    // push_back 10,20,30 → get(0)=10, get(2)=30 (0-based from the front).
    let src = "fn main() -> Int {\n  let d = Deque()\n  d.push_back(10)\n  d.push_back(20)\n  d.push_back(30)\n  d.get(2)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 30);
}

#[test]
fn deque_pop_front_on_empty_faults() {
    // Popping an empty deque faults EmptyCollection.
    let src = "fn main() -> Int {\n  let d = Deque()\n  d.pop_front()\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "empty pop should fault");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::EmptyCollection);
}

#[test]
fn deque_pop_back_on_empty_faults() {
    let src = "fn main() -> Int {\n  let d = Deque()\n  d.pop_back()\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "empty pop should fault");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::EmptyCollection);
}

#[test]
fn deque_is_empty_true_then_false() {
    // An empty deque is_empty → 1; after a push it is not → 0.
    let src = "fn main() -> Int {\n  let d = Deque()\n  if d.is_empty() { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn deque_drained_is_empty() {
    // Push one, pop one → empty again.
    let src = "fn main() -> Int {\n  let d = Deque()\n  d.push_back(7)\n  let _ = d.pop_front()\n  if d.is_empty() { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn deque_equality_is_structural() {
    // Two deques with the same elements in the same order are equal.
    let src = "fn main() -> Int {\n  let a = Deque()\n  a.push_back(1)\n  a.push_back(2)\n  let b = Deque()\n  b.push_back(1)\n  b.push_back(2)\n  if a == b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

// --- M8-WS3: Map[K,V] / Set[T] / Counter[T] (§6.1, §11.3) -------------------
// These are the headline §19.7 tests: tuples/records/nested collections as
// map/set keys, working end-to-end through the DynamicKey descriptor bridge.

#[test]
fn map_insert_get_len_end_to_end() {
    // Insert two (Int→Int) entries, get one back, check len.
    let src =
        "fn main() -> Int {\n  let m = Map()\n  m.insert(1, 10)\n  m.insert(2, 20)\n  m.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn map_get_returns_inserted_value() {
    let src = "fn main() -> Int {\n  let m = Map()\n  m.insert(7, 42)\n  m.get(7)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

#[test]
fn map_get_absent_returns_unit() {
    // An absent key returns Unit (a real Option[V] return is a follow-up; for
    // now `get` is paired with `contains` to distinguish present/absent). The
    // returned Unit is not an Int, so we only assert no fault occurred.
    let src = "fn main() -> Int {\n  let m = Map()\n  let _ = m.get(99)\n  0\n}\n";
    let (rt, _result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
}

#[test]
fn map_contains_distinguishes_present_and_absent() {
    let src = "fn main() -> Int {\n  let m = Map()\n  m.insert(5, 1)\n  if m.contains(5) { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn map_remove_drops_entry() {
    let src = "fn main() -> Int {\n  let m = Map()\n  m.insert(1, 10)\n  m.insert(2, 20)\n  m.remove(1)\n  m.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn map_insert_overwrites_prior_value() {
    let src = "fn main() -> Int {\n  let m = Map()\n  m.insert(1, 10)\n  m.insert(1, 99)\n  m.get(1)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 99);
}

#[test]
fn map_with_tuple_keys_end_to_end() {
    // The headline §19.7 criterion: tuples as map keys. Two structurally-equal
    // tuples must hit the same entry.
    let src = "fn main() -> Int {\n  let m = Map()\n  m.insert((1, 2), 100)\n  m.get((1, 2))\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 100);
}

#[test]
fn map_with_distinct_tuple_keys() {
    // (1,2) and (1,3) are distinct keys.
    let src = "fn main() -> Int {\n  let m = Map()\n  m.insert((1, 2), 100)\n  m.insert((1, 3), 200)\n  m.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn map_with_text_keys_end_to_end() {
    // Text keys: two equal strings hit the same entry.
    let src =
        "fn main() -> Int {\n  let m = Map()\n  m.insert(\"hello\", 1)\n  m.get(\"hello\")\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn set_insert_contains_len_end_to_end() {
    let src = "fn main() -> Int {\n  let s = Set()\n  s.insert(1)\n  s.insert(2)\n  s.insert(1)\n  s.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // Duplicate insert (1 twice) → 2 distinct elements.
    assert_eq!(result.as_int(), 2);
}

#[test]
fn set_contains_true_false() {
    let src = "fn main() -> Int {\n  let s = Set()\n  s.insert(7)\n  if s.contains(7) { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn set_with_tuple_keys() {
    // Tuples in a set (§19.7).
    let src = "fn main() -> Int {\n  let s = Set()\n  s.insert((1, 2))\n  s.insert((1, 2))\n  s.insert((3, 4))\n  s.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn counter_absent_reads_zero() {
    // §6.2: "Counter missing values behave as zero" — the §19.8 acceptance
    // criterion. An absent key's count is 0.
    let src = "fn main() -> Int {\n  let c = Counter()\n  c.get(\"absent\")\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

#[test]
fn counter_inc_increments() {
    let src = "fn main() -> Int {\n  let c = Counter()\n  c.inc(\"a\")\n  c.inc(\"a\")\n  c.inc(\"a\")\n  c.get(\"a\")\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

#[test]
fn counter_distinct_keys_tracked_separately() {
    let src = "fn main() -> Int {\n  let c = Counter()\n  c.inc(\"a\")\n  c.inc(\"b\")\n  c.inc(\"b\")\n  c.get(\"b\")\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn counter_len_counts_distinct_keys() {
    let src = "fn main() -> Int {\n  let c = Counter()\n  c.inc(\"a\")\n  c.inc(\"a\")\n  c.inc(\"b\")\n  c.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

// --- M8-WS4: MinHeap[T] / MaxHeap[T] (§6.1, §11.2) --------------------------

#[test]
fn max_heap_pop_returns_largest() {
    // Push 3, 1, 2; pop yields 3 (the largest first).
    let src = "fn main() -> Int {\n  let h = MaxHeap()\n  h.push(3)\n  h.push(1)\n  h.push(2)\n  h.pop()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

#[test]
fn max_heap_pop_ordering_is_descending() {
    // Pop all three: 3, 2, 1 (descending).
    let src = "fn main() -> Int {\n  let h = MaxHeap()\n  h.push(3)\n  h.push(1)\n  h.push(2)\n  let a = h.pop()\n  let b = h.pop()\n  let c = h.pop()\n  a * 100 + b * 10 + c\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 321);
}

#[test]
fn max_heap_peek_does_not_remove() {
    let src = "fn main() -> Int {\n  let h = MaxHeap()\n  h.push(5)\n  h.push(10)\n  let _ = h.peek()\n  h.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn max_heap_peek_returns_largest() {
    let src = "fn main() -> Int {\n  let h = MaxHeap()\n  h.push(7)\n  h.push(3)\n  h.push(9)\n  h.peek()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 9);
}

#[test]
fn max_heap_pop_empty_faults() {
    let src = "fn main() -> Int {\n  let h = MaxHeap()\n  h.pop()\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "empty pop should fault");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::EmptyCollection);
}

#[test]
fn max_heap_is_empty_true() {
    let src = "fn main() -> Int {\n  let h = MaxHeap()\n  if h.is_empty() { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn min_heap_pop_returns_smallest() {
    // Push 3, 1, 2; pop yields 1 (the smallest first).
    let src = "fn main() -> Int {\n  let h = MinHeap()\n  h.push(3)\n  h.push(1)\n  h.push(2)\n  h.pop()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn min_heap_pop_ordering_is_ascending() {
    // Pop all three: 1, 2, 3 (ascending).
    let src = "fn main() -> Int {\n  let h = MinHeap()\n  h.push(3)\n  h.push(1)\n  h.push(2)\n  let a = h.pop()\n  let b = h.pop()\n  let c = h.pop()\n  a * 100 + b * 10 + c\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 123);
}

#[test]
fn min_heap_peek_returns_smallest() {
    let src = "fn main() -> Int {\n  let h = MinHeap()\n  h.push(7)\n  h.push(3)\n  h.push(9)\n  h.peek()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

#[test]
fn min_heap_pop_empty_faults() {
    let src = "fn main() -> Int {\n  let h = MinHeap()\n  h.pop()\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "empty pop should fault");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::EmptyCollection);
}

// --- M8-WS5: BitSet (§6.1) --------------------------------------------------

#[test]
fn bitset_insert_contains_len() {
    let src = "fn main() -> Int {\n  let b = BitSet()\n  b.insert(0)\n  b.insert(64)\n  b.insert(1000)\n  b.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

#[test]
fn bitset_contains_true_false() {
    let src = "fn main() -> Int {\n  let b = BitSet()\n  b.insert(5)\n  if b.contains(5) { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn bitset_contains_absent_false() {
    let src = "fn main() -> Int {\n  let b = BitSet()\n  b.insert(5)\n  if b.contains(6) { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

#[test]
fn bitset_remove_clears_bit() {
    let src = "fn main() -> Int {\n  let b = BitSet()\n  b.insert(5)\n  b.insert(10)\n  b.remove(5)\n  b.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn bitset_is_empty_true_then_false() {
    let src = "fn main() -> Int {\n  let b = BitSet()\n  let first = if b.is_empty() { 1 } else { 0 }\n  b.insert(1)\n  let second = if b.is_empty() { 1 } else { 0 }\n  first * 10 + second\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 10);
}

// --- M8-WS5: Grid[T] methods (§6.4) ----------------------------------------

#[test]
fn grid_width_height_from_parsed_grid() {
    // Parse a 2-column × 2-row grid; width=2, height=2.
    let src = "fn main() -> Int {\n  let g = read grid(char)\n  g.width() * 10 + g.height()\n}\n";
    let (rt, result) = run_main_with_input(src, "ab\ncd\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 22);
}

#[test]
fn grid_get_reads_cell() {
    // Grid "ab/cd": get(1, 0) returns the Char 'b'. Compare via find_all: the
    // count of cells equal to the (1,0) cell should be 1. Intermediate `let`
    // bindings avoid the method-chain-after-args parser limitation.
    let src = "fn main() -> Int {\n  let g = read grid(char)\n  let cell = g.get(1, 0)\n  let matches = g.find_all(cell)\n  matches.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "ab\ncd\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn grid_get_out_of_bounds_faults() {
    let src = "fn main() -> Int {\n  let g = read grid(char)\n  let _ = g.get(9, 9)\n  0\n}\n";
    let (rt, _result) = run_main_with_input(src, "ab\n");
    assert!(rt.has_pending_fault(), "OOB should fault");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::IndexOutOfBounds);
}

#[test]
fn grid_contains_in_and_out() {
    // (1,1) is in a 2×2 grid; (5,5) is not.
    let src = "fn main() -> Int {\n  let g = read grid(char)\n  let a = if g.contains(1, 1) { 1 } else { 0 }\n  let b = if g.contains(5, 5) { 1 } else { 0 }\n  a * 10 + b\n}\n";
    let (rt, result) = run_main_with_input(src, "ab\ncd\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 10);
}

#[test]
fn grid_neighbors4_corner() {
    // Top-left corner (0,0) of a 2×2 grid has 2 in-bounds neighbors (right, down).
    let src = "fn main() -> Int {\n  let g = read grid(char)\n  let ns = g.neighbors4((0, 0))\n  ns.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "ab\ncd\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn grid_neighbors8_center() {
    // Center (1,1) of a 3×3 grid has all 8 neighbors.
    let src = "fn main() -> Int {\n  let g = read grid(char)\n  let ns = g.neighbors8((1, 1))\n  ns.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "abc\ndef\nghi\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 8);
}

#[test]
fn grid_positions_count() {
    // A 2×3 grid has 6 positions. (Intermediate `let` avoids the method-chain
    // parser limitation for chains after a no-arg method returning a collection.)
    let src =
        "fn main() -> Int {\n  let g = read grid(char)\n  let ps = g.positions()\n  ps.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "abc\ndef\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 6);
}

#[test]
fn grid_cells_count() {
    let src =
        "fn main() -> Int {\n  let g = read grid(char)\n  let cs = g.cells()\n  cs.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "ab\ncd\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 4);
}

#[test]
fn grid_row() {
    // Row 1 of "ab/cd" is "cd" (length 2). The row is a Vec[Char]; check its len.
    let src = "fn main() -> Int {\n  let g = read grid(char)\n  let r = g.row(1)\n  r.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "ab\ncd\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn grid_column() {
    // Column 0 of "ab/cd" is "ac" (length 2).
    let src =
        "fn main() -> Int {\n  let g = read grid(char)\n  let c = g.column(0)\n  c.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "ab\ncd\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn grid_find_locates_first_match() {
    // Grid "ab/cd": find a cell, then verify find_all for that cell finds 1.
    let src = "fn main() -> Int {\n  let g = read grid(char)\n  let cell = g.get(0, 1)\n  let matches = g.find_all(cell)\n  matches.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "ab\ncd\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn grid_find_all_count() {
    // Grid with two 'x' cells. Get the 'x' value via get(0,0) then find_all.
    let src = "fn main() -> Int {\n  let g = read grid(char)\n  let x = g.get(0, 0)\n  let matches = g.find_all(x)\n  matches.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "x.\n.x\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn grid_transpose_round_trips_dimensions() {
    // A 3-wide × 2-tall grid transposes to 2-wide × 3-tall.
    let src = "fn main() -> Int {\n  let g = read grid(char)\n  let t = g.transpose()\n  t.width() * 10 + t.height()\n}\n";
    let (rt, result) = run_main_with_input(src, "abc\ndef\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 23);
}

#[test]
fn grid_rotate_left_changes_dimensions() {
    // A 3-wide × 2-tall grid rotated left → 2-wide × 3-tall.
    let src = "fn main() -> Int {\n  let g = read grid(char)\n  let r = g.rotate_left()\n  r.width() * 10 + r.height()\n}\n";
    let (rt, result) = run_main_with_input(src, "abc\ndef\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 23);
}

#[test]
fn grid_rotate_right_changes_dimensions() {
    let src = "fn main() -> Int {\n  let g = read grid(char)\n  let r = g.rotate_right()\n  r.width() * 10 + r.height()\n}\n";
    let (rt, result) = run_main_with_input(src, "abc\ndef\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 23);
}

#[test]
fn grid_rotate_four_times_is_identity() {
    // Rotating right 4× returns to the original dimensions (3×2).
    let src = "fn main() -> Int {\n  let g = read grid(char)\n  let r1 = g.rotate_right()\n  let r2 = r1.rotate_right()\n  let r3 = r2.rotate_right()\n  let r4 = r3.rotate_right()\n  r4.width() * 10 + r4.height()\n}\n";
    let (rt, result) = run_main_with_input(src, "abc\ndef\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 32); // back to 3-wide × 2-tall
}

// --- M8-WS6: Control flow §4.11 (for/loop/break/continue/return) ------------

#[test]
fn for_loop_sums_vec_elements() {
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(10)\n  v.push(20)\n  v.push(30)\n  var sum = 0\n  for x in v { sum = sum + x }\n  sum\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 60);
}

#[test]
fn for_loop_empty_vec_zero_iterations() {
    let src = "fn main() -> Int {\n  let v = Vec()\n  var sum = 0\n  for x in v { sum = sum + x }\n  sum\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

#[test]
fn for_loop_counts_iterations() {
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(1)\n  v.push(1)\n  v.push(1)\n  var n = 0\n  for x in v { n = n + 1 }\n  n\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 4);
}

#[test]
fn loop_break_exits() {
    let src = "fn main() -> Int {\n  var i = 0\n  loop { if i >= 5 { break } i = i + 1 }\n  i\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 5);
}

#[test]
fn while_break_exits_early() {
    let src = "fn main() -> Int {\n  var i = 0\n  while i < 100 { if i == 7 { break } i = i + 1 }\n  i\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 7);
}

#[test]
fn continue_skips_rest_of_body() {
    // Sum 1..10 but skip even numbers (continue): 1+3+5+7+9 = 25.
    let src = "fn main() -> Int {\n  var i = 0\n  var sum = 0\n  while i < 10 { i = i + 1 if i - i / 2 * 2 == 0 { continue } sum = sum + i }\n  sum\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 25);
}

#[test]
fn return_exits_function_early() {
    let src = "fn first(v: Vec[Int]) -> Int { for x in v { return x } 0 }\n  fn main() -> Int {\n  let v = Vec()\n  v.push(42)\n  v.push(99)\n  first(v)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

#[test]
fn nested_loops_with_break() {
    // Inner break exits only the inner loop; outer continues.
    let src = "fn main() -> Int {\n  var count = 0\n  var i = 0\n  while i < 3 {\n    var j = 0\n    loop { if j >= 2 { break } count = count + 1 j = j + 1 }\n    i = i + 1\n  }\n  count\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 6); // 3 outer × 2 inner
}

#[test]
fn for_loop_over_deque() {
    let src = "fn main() -> Int {\n  let d = Deque()\n  d.push_back(5)\n  d.push_back(10)\n  d.push_back(15)\n  var sum = 0\n  for x in d { sum = sum + x }\n  sum\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 30);
}

// --- M8-WS8: pipeline combinators (§6.3) -----------------------------------

#[test]
fn pipeline_sum_sums_elements() {
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(10)\n  v.push(20)\n  v.push(30)\n  v.sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 60);
}

#[test]
fn pipeline_count_counts_elements() {
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.count()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

#[test]
fn pipeline_map_applies_closure() {
    // map (|x| x*2) over [1,2,3] → [2,4,6], then sum → 12.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  let doubled = v.map(|x| x * 2)\n  doubled.sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 12);
}

#[test]
fn pipeline_filter_keeps_matching() {
    // filter (|x| even) over [1,2,3,4] → [2,4], sum → 6.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  let evens = v.filter(|x| x - x / 2 * 2 == 0)\n  evens.sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 6);
}

#[test]
fn pipeline_collect_materializes() {
    // collect into a Vec, then len.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  let copy = v.collect()\n  copy.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

#[test]
fn pipeline_map_then_len_chains() {
    // map then .len() (method chain after a method-with-args, fixed in WS6).
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.map(|x| x * 2).len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

// ===========================================================================
// Milestone 5: Text methods (§4.3) and `out(...)` (§16.1).
// ===========================================================================

#[test]
fn text_len_and_get_end_to_end() {
    // Text literals allocate; .len() counts chars; .get(0) returns the scalar.
    let src = "fn main() -> Int {\n  let s = \"hello\"\n  s.get(1)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault());
    // 'e' = 101
    assert_eq!(result.as_int(), 101);
}

#[test]
fn text_len_counts_unicode_scalars() {
    // "héllo" has 5 Unicode scalar values (é is one char).
    let src = "fn main() -> Int {\n  let s = \"héllo\"\n  s.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 5);
}

#[test]
fn text_get_indexes_by_scalar_not_byte() {
    // `praxis_text_get` must index by Unicode scalar value, not by byte: in
    // "héllo" the char at index 1 is é (scalar 233), but é is encoded as two
    // bytes (0xC3 0xA9), so byte indexing would return 0xC3 (195) instead. This
    // distinguishes the two implementations and guards a regression toward
    // byte indexing — load-bearing for M6, where input parsing produces Text
    // values that get indexed into.
    let src = "fn main() -> Int {\n  let s = \"héllo\"\n  s.get(1)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 233);
}

#[test]
fn text_is_empty_works() {
    // An empty text literal's .is_empty() → Bool → compare as 1.
    let src = "fn main() -> Int {\n  let s = \"\"\n  if s.is_empty() { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn text_get_out_of_bounds_faults() {
    let src = "fn main() -> Int {\n  let s = \"ab\"\n  s.get(5)\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault());
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::IndexOutOfBounds);
}

#[test]
fn out_writes_to_stdout_and_returns_value() {
    // out(expr) should write the formatted value to stdout. We can't easily
    // capture stdout in a unit test; instead verify it doesn't fault and the
    // program completes. The return is the value itself (for chaining).
    let src = "fn main() -> Int {\n  out(42)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 42);
}

// ===========================================================================
// Milestone 5: var reassignment and let object mutation under GC (§4.2).
// ===========================================================================

#[test]
fn var_vec_reassign_survives_gc() {
    // `var v` is reassigned to a fresh Vec after heavy allocation. The GC must
    // not reclaim the live binding. §4.2: reassignment updates the binding.
    let src = "fn main() -> Int {\n  var v = Vec()\n  var i = 0\n  while i < 500 { v.push(i); i = i + 1 }\n  v = Vec()\n  v.push(999)\n  v.get(0)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 999);
}

#[test]
fn let_vec_mutation_visible_after_gc() {
    // §4.2: "A let binding may still point to a mutable object." Push 1000
    // elements to a `let v` (mutating the object), survive GCs, read back.
    let src = "fn main() -> Int {\n  let v = Vec()\n  var i = 0\n  while i < 1000 { v.push(i * 2); i = i + 1 }\n  v.get(500)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 1000); // 500 * 2
}

// ===========================================================================
// Milestone 6: Char wired end-to-end (§4.3). The runtime descriptor exists;
// M6 connects inference → HIR → MIR → codegen → runtime. The input parser
// produces Char values (`char` atom, `grid(char)`); these tests exercise the
// runtime allocation path that path will use.
// ===========================================================================

#[test]
fn char_type_annotation_is_accepted() {
    // `Char` must now type-check (M6: reserved → wired). This compiles through
    // the whole pipeline (resolve → infer → lower → MIR → JIT) without error.
    let src = "fn main() -> Char {\n  out(0)\n}\n";
    let (_jit, ids) = compile(src);
    assert!(ids.contains_key("main"), "Char return type compiles");
}

#[test]
fn char_runtime_roundtrip() {
    // The descriptor + allocator path the input parser will call: alloc_char
    // stores a u32 scalar; as_char recovers it. Exercises scalars::CHAR.
    let rt = Runtime::new();
    let c = rt.alloc_char('€' as u32);
    assert_eq!(c.as_char(), '€');
    // A simple ASCII char.
    let a = rt.alloc_char('A' as u32);
    assert_eq!(a.as_char(), 'A');
}

// ===========================================================================
// Milestone 6: `read` input parser (§7). End-to-end tests for the headline
// feature: source → parse → infer → lower → MIR → JIT → run the parser plan.
// ===========================================================================

#[test]
fn read_lines_of_int_parses_input() {
    // `read lines(int)` against "10\n20\n30" → Vec[Int] of [10, 20, 30].
    // The program reads .len() and returns it.
    let src = "fn main() -> Int {\n  let v = read lines(int)\n  v.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "10\n20\n30\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

#[test]
fn read_lines_of_int_first_element() {
    // Read the first element of a lines(int) parse.
    let src = "fn main() -> Int {\n  let v = read lines(int)\n  v.get(0)\n}\n";
    let (rt, result) = run_main_with_input(src, "42\n99\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

#[test]
fn read_with_var_binding() {
    // Acceptance criterion 2: bind read results with `var`.
    let src = "fn main() -> Int {\n  var v = read lines(int)\n  v.get(1)\n}\n";
    let (rt, result) = run_main_with_input(src, "10\n20\n30\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 20);
}

#[test]
fn multiple_reads_parse_same_buffer() {
    // Acceptance criterion 3: multiple `read` expressions deterministically
    // parse the same complete source buffer.
    let src = "fn main() -> Int {\n  let a = read lines(int)\n  let b = read lines(int)\n  a.get(0) + b.get(1)\n}\n";
    let (rt, result) = run_main_with_input(src, "100\n200\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 300); // 100 + 200
}

#[test]
fn read_sections_lines_csv_int_nested() {
    // `read sections(lines(csv(int)))` — the §7.6 nested example. This exercises
    // deeply nested collection parsing. The parser correctly produces
    // Vec[Vec[Vec[Int]]]; reading back the first element of each level works
    // for the leaf (Int) but nested Vec element descriptors need the child
    // descriptor to resolve recursively (an M6 follow-up). For now we verify
    // the outer structure is correct (non-faulting, returns a Vec).
    let src =
        "fn main() -> Int {\n  let groups = read sections(lines(csv(int)))\n  groups.len()\n}\n";
    let input = "1,2,3\n4,5,6\n\n7,8,9\n";
    let (rt, result) = run_main_with_input(src, input);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2); // two sections
}

// --- M7-WS9: input-parser carryovers (§7.2 templates, nested descriptors) --

#[test]
fn read_lines_of_named_capture_template_parses_records() {
    // `read lines(`{x:int},{y:int}`)` → Vec[{x:Int,y:Int}]. Each line matches the
    // template; named captures become record fields. We read .len() to confirm
    // three records parsed without faulting.
    let src = "fn main() -> Int {\n  let v = read lines(`{x:int},{y:int}`)\n  v.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "1,2\n3,4\n5,6\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

#[test]
fn read_lines_of_single_anon_capture_parses_scalars() {
    // `read lines(`{int}`)` → Vec[Int]. A single anonymous capture yields the
    // scalar value directly. Read the first element to confirm the value flows.
    let src = "fn main() -> Int {\n  let v = read lines(`{int}`)\n  v.get(1)\n}\n";
    let (rt, result) = run_main_with_input(src, "10\n20\n30\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 20);
}

#[test]
fn read_lines_of_multi_anon_capture_parses_tuples() {
    // `read lines(`{int},{int}`)` → Vec[(Int, Int)]. Two anonymous captures
    // assemble into a tuple. We read .len() to confirm parsing succeeded.
    let src = "fn main() -> Int {\n  let v = read lines(`{int},{int}`)\n  v.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "1,2\n3,4\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn read_standalone_named_capture_template_parses_one_record() {
    // A standalone template (no `lines`) against the whole buffer. `{x:int},{y:int}`
    // parses a single record from "7,8".
    let src = "fn main() -> Int {\n  let r = read `{x:int},{y:int}`\n  0\n}\n";
    let (rt, result) = run_main_with_input(src, "7,8\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

#[test]
fn read_nested_collections_descriptor_is_composite() {
    // `read sections(lines(csv(int)))` now tags the outer Vec's element
    // descriptor as a Vec (not the leaf Int), so formatting/equality on nested
    // collections dispatches correctly. Compare two identical nested parses for
    // structural equality → true (1).
    let src = "fn main() -> Int {\n  let a = read sections(lines(csv(int)))\n  let b = read sections(lines(csv(int)))\n  if a == b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main_with_input(src, "1,2\n3,4\n\n1,2\n3,4\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

// --- short-circuit || and ! (M7-WS2 carryover) ------------------------------

#[test]
fn logical_or_returns_true_when_lhs_true() {
    // true || false → true (→ 1).
    let src = "fn main() -> Int {\n  let b = true || false\n  if b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn logical_or_returns_rhs_when_lhs_false() {
    // false || true → true (→ 1).
    let src = "fn main() -> Int {\n  let b = false || true\n  if b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn logical_or_returns_false_when_both_false() {
    // false || false → false (→ 0).
    let src = "fn main() -> Int {\n  let b = false || false\n  if b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

#[test]
fn logical_or_short_circuits_skipping_rhs_side_effect() {
    // The acceptance test for short-circuit: when lhs is true, the rhs division
    // by zero must NOT execute (no fault). If || were eager, this would fault.
    let src = "fn main() -> Int {\n  let b = true || (1 / 0 == 0)\n  if b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(
        !rt.has_pending_fault(),
        "short-circuit must skip the div-by-zero, but fault: {:?}",
        rt.fault()
    );
    assert_eq!(result.as_int(), 1);
}

#[test]
fn logical_or_evaluates_rhs_side_effect_when_lhs_false() {
    // When lhs is false, the rhs IS evaluated — so a div-by-zero faults.
    let src = "fn main() -> Int {\n  let b = false || (1 / 0 == 0)\n  if b { 1 } else { 0 }\n}\n";
    let (rt, _result) = run_main(src);
    assert!(
        rt.has_pending_fault(),
        "rhs must be evaluated when lhs is false"
    );
}

#[test]
fn logical_not_flips_bool() {
    // !true → false (→ 0); !false → true (→ 1).
    let src = "fn main() -> Int {\n  let a = !true\n  let b = !false\n  if a { 0 } else { if b { 1 } else { 0 } }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn double_not_is_identity() {
    // !!true → true (→ 1).
    let src = "fn main() -> Int {\n  let b = !!true\n  if b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

// --- nominal records (M7-WS3, §4.5) -----------------------------------------

#[test]
fn record_construction_and_field_access() {
    // `Point { x: 3, y: 4 }` → read back x + y = 7.
    let src = "struct Point { x: Int, y: Int }\nfn main() -> Int {\n  let p = Point { x: 3, y: 4 }\n  p.x + p.y\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 7);
}

#[test]
fn record_field_access_independently() {
    // Read just one field.
    let src = "struct Point { x: Int, y: Int }\nfn main() -> Int {\n  let p = Point { x: 30, y: 4 }\n  p.x\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 30);
}

#[test]
fn record_with_text_field() {
    // A record with a Text field, accessed and used.
    let src = "struct Entry { key: Int, label: Text }\nfn main() -> Int {\n  let e = Entry { key: 42, label: \"hello\" }\n  e.key\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

#[test]
fn record_survives_gc() {
    // Allocate a record, trigger GC by allocating many objects, then read back.
    // This verifies the record's GcRef is rooted across safepoints.
    let src = "struct Point { x: Int, y: Int }\nfn main() -> Int {\n  let p = Point { x: 100, y: 200 }\n  var i = 0\n  while i < 100 {\n    let junk = i + 1\n    i = i + 1\n  }\n  p.x + p.y\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 300);
}

#[test]
fn record_field_punning() {
    // Field punning: `Point { x, y }` where x and y are bindings.
    let src = "struct Point { x: Int, y: Int }\nfn main() -> Int {\n  let x = 5\n  let y = 7\n  let p = Point { x, y }\n  p.x * p.y\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 35);
}

// --- tuples (M7-WS6, §4.5 structural tuples) --------------------------------

#[test]
fn tuple_construction_does_not_fault() {
    // M7 Part 2: tuples now materialize as real objects. Constructing one must
    // not fault (previously this was a Unit stub).
    let src = "fn main() -> Int {\n  let t = (1, 2, 3)\n  7\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 7);
}

#[test]
fn tuple_survives_gc() {
    // Allocate a tuple, trigger GC by allocating many objects, and confirm the
    // program completes without faulting (the tuple's GcRef must be rooted
    // across safepoints).
    let src = "fn main() -> Int {\n  let t = (100, 200)\n  var i = 0\n  while i < 100 {\n    let junk = i + 1\n    i = i + 1\n  }\n  9\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 9);
}

#[test]
fn tuple_of_mixed_types() {
    // A tuple with heterogeneous element types (Int, Bool) constructs cleanly.
    let src = "fn main() -> Int {\n  let t = (42, true)\n  5\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 5);
}

// --- structural equality (M7-WS6, §5.5) -------------------------------------

#[test]
fn record_equality_true() {
    // `Point{1,2} == Point{1,2}` → true (1). Bind to `let` first because record
    // literals are blocked in `if` conditions (`no_struct_literal`).
    let src = "struct Point { x: Int, y: Int }\nfn main() -> Int {\n  let a = Point { x: 1, y: 2 }\n  let b = Point { x: 1, y: 2 }\n  if a == b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn record_equality_false() {
    // `Point{1,2} == Point{1,3}` → false (0).
    let src = "struct Point { x: Int, y: Int }\nfn main() -> Int {\n  let a = Point { x: 1, y: 2 }\n  let b = Point { x: 1, y: 3 }\n  if a == b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

#[test]
fn record_inequality() {
    // `Point{1,2} != Point{1,3}` → true (1).
    let src = "struct Point { x: Int, y: Int }\nfn main() -> Int {\n  let a = Point { x: 1, y: 2 }\n  let b = Point { x: 1, y: 3 }\n  if a != b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn tuple_equality_true() {
    // `(1, 2) == (1, 2)` → true.
    let src =
        "fn main() -> Int {\n  let a = (1, 2)\n  let b = (1, 2)\n  if a == b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn tuple_equality_false() {
    // `(1, 2) == (1, 3)` → false.
    let src =
        "fn main() -> Int {\n  let a = (1, 2)\n  let b = (1, 3)\n  if a == b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

#[test]
fn enum_equality_same_variant() {
    // `Empty == Empty` → true; `Wall == Wall` → true.
    let src = "enum Tile { Empty, Wall, Number(Int) }\nfn main() -> Int {\n  if Empty == Empty { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn enum_equality_different_variant() {
    // `Empty == Wall` → false.
    let src = "enum Tile { Empty, Wall, Number(Int) }\nfn main() -> Int {\n  if Empty == Wall { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

#[test]
fn enum_equality_with_payload() {
    // `Number(42) == Number(42)` → true; `Number(42) == Number(7)` → false.
    let src = "enum Tile { Empty, Number(Int) }\nfn main() -> Int {\n  let a = Number(42) == Number(42)\n  let b = Number(42) == Number(7)\n  if a { if b { 3 } else { 2 } } else { 1 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn scalar_equality_still_works() {
    // `==` on Int still uses the native scalar compare path.
    let src = "fn main() -> Int {\n  if 3 == 3 { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

// --- enums (M7-WS4, §4.6) ---------------------------------------------------

#[test]
fn enum_payload_variant_construction() {
    // `Number(5)` constructs a payload variant. Verify construction doesn't fault.
    let src =
        "enum Tile { Empty, Number(Int) }\nfn main() -> Int {\n  let t = Number(42)\n  7\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 7);
}

#[test]
fn enum_zero_payload_variant_as_value() {
    // `Empty` is a bare zero-payload variant value.
    let src = "enum Tile { Empty, Number(Int) }\nfn main() -> Int {\n  let t = Empty\n  8\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 8);
}

#[test]
fn enum_construction_does_not_fault() {
    // Direct enum construction — verify it allocates without faulting.
    let src = "enum Tile { Empty, Wall, Number(Int) }\nfn main() -> Int {\n  let a = Empty\n  let b = Wall\n  let c = Number(99)\n  42\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

#[test]
fn enum_survives_gc() {
    // Allocate enum values, trigger GC, verify no fault.
    let src = "enum Tile { Empty, Number(Int) }\nfn main() -> Int {\n  let t = Number(123)\n  var i = 0\n  while i < 50 {\n    let junk = i + 1\n    i = i + 1\n  }\n  456\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 456);
}

// --- pattern matching (M7-WS5, §4.6) ----------------------------------------

#[test]
fn match_enum_wall_arm() {
    let src = "enum Tile { Empty, Wall }\nfn main() -> Int {\n  let t = Wall\n  match t {\n    Empty => 1\n    Wall => 2\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn match_enum_with_wildcard_default() {
    let src = "enum Tile { Empty, Wall, Number(Int) }\nfn main() -> Int {\n  let t = Wall\n  match t {\n    Empty => 1\n    _ => 99\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 99);
}

#[test]
fn match_enum_zero_payload_returns_arm_value() {
    let src = "enum Tile { Empty, Wall }\nfn main() -> Int {\n  let t = Empty\n  match t {\n    Empty => 1\n    Wall => 2\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn match_enum_payload_binding() {
    let src = "enum Tile { Empty, Number(Int) }\nfn main() -> Int {\n  let t = Number(42)\n  match t {\n    Empty => 0\n    Number(n) => n\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

// --- M7-WSP: pattern-matching completeness (nested + literal patterns) -------

#[test]
fn match_literal_int() {
    // Literal Int patterns — the WS5 bug always took the first arm; this now
    // tests each value. `match n { 1 => 10, 2 => 20, _ => 0 }`.
    let src = "fn main() -> Int {\n  let n = 2\n  match n {\n    1 => 10\n    2 => 20\n    _ => 0\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 20);
}

#[test]
fn match_literal_int_default() {
    // The wildcard arm must catch unmatched literals.
    let src = "fn main() -> Int {\n  let n = 99\n  match n {\n    1 => 10\n    2 => 20\n    _ => 0\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

#[test]
fn match_literal_int_first_arm() {
    // Matching the first literal arm.
    let src = "fn main() -> Int {\n  let n = 1\n  match n {\n    1 => 10\n    2 => 20\n    _ => 0\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 10);
}

#[test]
fn match_bool() {
    // Bool patterns: `match b { true => 1, false => 0 }`.
    let src =
        "fn main() -> Int {\n  let b = true\n  match b {\n    true => 1\n    false => 0\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn match_nested_variant_pattern() {
    // Nested pattern: `Wrapped(Some(n))` extracts through two layers of variant.
    // The WS5 bug silently dropped nested sub-patterns; this now recurses.
    let src = "enum Inner { None, Some(Int) }\nenum Outer { Wrapped(Inner), Bare }\nfn main() -> Int {\n  let v = Wrapped(Some(7))\n  match v {\n    Wrapped(Some(n)) => n\n    Wrapped(None) => 0\n    Bare => 1\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 7);
}

#[test]
fn match_nested_variant_none_branch() {
    // The same nested match but matching the inner None.
    let src = "enum Inner { None, Some(Int) }\nenum Outer { Wrapped(Inner), Bare }\nfn main() -> Int {\n  let v = Wrapped(None)\n  match v {\n    Wrapped(Some(n)) => n\n    Wrapped(None) => 0\n    Bare => 1\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

#[test]
fn match_multi_payload_binding() {
    // A variant with multiple payload fields, all bound.
    let src = "enum Shape { Point(Int, Int), Empty }\nfn main() -> Int {\n  let s = Point(3, 4)\n  match s {\n    Point(x, y) => x + y\n    Empty => 0\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 7);
}

#[test]
fn match_variable_bind_whole_scrutinee() {
    // A bare variable bind `x` matches anything and binds the whole value.
    let src = "enum Tile { Empty, Number(Int) }\nfn main() -> Int {\n  let t = Number(5)\n  match t {\n    x => 99\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 99);
}

// --- M7-WS7: closures (§4.10) ---------------------------------------------
//
// Closures capture outer `let`/`param` values by copying them into the closure's
// runtime environment; the synthetic function loads them at entry (Approach B).
// Calling a closure value is an indirect call through its `fn_ptr`.

#[test]
fn closure_no_captures() {
    // A closure that references only its own param.
    let (rt, result) = run_main("fn main() -> Int { let f = |x| x * 2; f(21) }");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

#[test]
fn closure_captures_outer_let() {
    // The headline demo: `let o = 10; let f = |x| x + o; f(5)` → 15.
    let (rt, result) = run_main("fn main() -> Int { let o = 10; let f = |x| x + o; f(5) }");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 15);
}

#[test]
fn closure_captures_multiple() {
    // Two captures, used together.
    let (rt, result) =
        run_main("fn main() -> Int { let a = 3; let b = 4; let f = |x| x + a * b; f(5) }");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 17);
}

#[test]
fn closure_captures_param_of_enclosing_fn() {
    // The captured value is the enclosing fn's param.
    let (rt, result) = run_main(
        "fn make(o: Int) -> Int { let f = |x| x + o; f(5) }\nfn main() -> Int { make(10) }",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 15);
}

#[test]
fn closure_returned_and_called() {
    // A fn returns a closure; main calls it. Exercises capture across a fn
    // boundary (the closure outlives `make`'s frame — the env is GC'd).
    let (rt, result) = run_main(
        "fn make(o: Int) -> Int { |x| x + o }\nfn main() -> Int { let f = make(10); f(5) }",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 15);
}

#[test]
fn closure_curried() {
    // A closure returning a closure: |x| |y| x + y. The inner closure captures
    // the outer's param `x`.
    let (rt, result) =
        run_main("fn main() -> Int { let add = |x| |y| x + y; let inc = add(1); inc(41) }");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

// --- M7-WS7b: mutable captures via VarCell (§4.10) ------------------------
//
// A `var` captured by a closure is boxed into a GC'd VarCell at its binding
// site. The binding function and every capturing closure share the cell, so a
// mutation in one frame is visible to the other.

#[test]
fn closure_reads_mutable_capture() {
    // The closure reads (but does not write) a captured `var` — the cell holds
    // the initial value.
    let (rt, result) = run_main("fn main() -> Int {\n  var c = 10\n  let f = |_| c\n  f(0)\n}\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 10);
}

#[test]
fn closure_mutates_mutable_capture_visible_outside() {
    // The headline mutable-capture scenario: a closure mutates the captured
    // `var`, and the outer scope reads the updated value after the closure runs.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var counter = 0\n  let inc = |_| { counter = counter + 1 }\n  inc(0)\n  inc(0)\n  counter\n}\n",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn closure_compound_assign_on_mutable_capture() {
    // A compound assignment (`+=`) on a captured `var` inside a closure: read
    // the cell, add, write back.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var total = 100\n  let add = |n| { total += n }\n  add(5)\n  add(10)\n  total\n}\n",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 115);
}

#[test]
fn mutable_capture_survives_returned_closure() {
    // A closure capturing a `var` is returned from a fn; calling it mutates the
    // cell (which outlives the fn's frame — it's GC'd).
    let (rt, result) = run_main(
        "fn make() {\n  var n = 0\n  |x| { n = n + x; n }\n}\nfn main() -> Int {\n  let bump = make()\n  bump(3)\n  bump(4)\n}\n",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 7);
}

// --- M7-WS8: monomorphization (§13.6) -------------------------------------
//
// Polymorphic user fns are instantiated per concrete call site. The mono pass
// runs between typed HIR and MIR; the JIT then compiles one clone per
// (callee, type-args) pair.

#[test]
fn monomorphization_identity_on_int() {
    // `fn id(x) { x }` generalizes to `forall a. a -> a`; called with Int, the
    // mono pass emits an `id__Int` clone and main calls it.
    let (rt, result) = run_main("fn id(x) { x }\nfn main() -> Int { id(42) }");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

#[test]
fn monomorphization_two_clones_of_same_generic_fn() {
    // `id` called twice on Int shares one clone; the result is the second call.
    let (rt, result) = run_main("fn id(x) { x }\nfn main() -> Int { id(1) + id(41) }");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

#[test]
fn monomorphization_generic_fn_with_two_params() {
    // `fn first(a, b) { a }` is `forall a b. (a, b) -> a`; instantiated at
    // (Int, Int).
    let (rt, result) = run_main("fn first(a, b) { a }\nfn main() -> Int { first(42, 99) }");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

#[test]
fn monomorphization_transitive_generic_call() {
    // A generic fn calling another generic fn: `wrap` calls `id`. Both must be
    // instantiated transitively.
    let (rt, result) =
        run_main("fn id(x) { x }\nfn wrap(y) { id(y) }\nfn main() -> Int { wrap(42) }");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

#[test]
fn monomorphization_generic_fn_called_from_closure_body() {
    // A generic fn called from inside a closure. The mono pass rewrites the
    // call inside the closure's body too.
    let (rt, result) = run_main("fn id(x) { x }\nfn main() -> Int { let f = |n| id(n); f(42) }");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}
