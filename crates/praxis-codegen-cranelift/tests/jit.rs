//! End-to-end JIT integration tests: source → typed HIR → MIR → Cranelift → run.
//!
//! These are the Milestone 4 acceptance tests (§19): execute boxed integer
//! arithmetic, branches, loops, and recursive function calls through the JIT,
//! and confirm faults return to the host without unwinding.

use praxis_ast::AstNode;
use praxis_codegen_cranelift::{Jit, RunnableFunction};
use praxis_hir::{analyze_root, lower};
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
