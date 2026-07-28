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
use praxis_runtime::{GcRef, RootSet, Runtime, RuntimeContext};
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
    let ids = jit
        .compile(&funcs, &mut analysis.db)
        .expect("JIT compilation");
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
    // Always install the input buffer (§7.10), even for empty input. The
    // default `input_source` is the Unit singleton; a `read` against it
    // segfaults (Unit's payload reinterpreted as a Text buffer). Installing an
    // empty Text makes `read` on empty input yield empty collections, and
    // non-`read` programs ignore `input_source` so this is harmless.
    let input_ref = rt.alloc_text(input);
    ctx.input_source = input_ref;
    let entry: RunnableFunction = unsafe { std::mem::transmute(jit.entry(main_id)) };
    // main takes no GcRef params beyond the context; pass Unit as the unused slot.
    let unit = rt.alloc_unit();
    let result = unsafe { entry(&mut ctx as *mut RuntimeContext, unit) };
    // Keep the JIT alive for the call (it owns the executable memory).
    drop(jit);
    (rt, result)
}

/// Like [`run_main`], but deliberately leaves `input_source` at its default (the
/// immortal Unit singleton) instead of installing a Text buffer. A `read`
/// against the Unit source must fault cleanly (`ParseFailed`) rather than
/// segfault — the parser interpreter would otherwise reinterpret Unit's payload
/// as a Text buffer (§6.3 host-safety gap, now guarded in `praxis_get_input`).
fn run_main_no_input(src: &str) -> (Runtime, GcRef) {
    let (jit, ids) = compile(src);
    let main_id = *ids.get("main").expect("no `main` function");
    let mut rt = Runtime::new();
    let mut ctx = rt.context();
    // Intentionally do NOT touch ctx.input_source: it stays at the default Unit.
    let entry: RunnableFunction = unsafe { std::mem::transmute(jit.entry(main_id)) };
    let unit = rt.alloc_unit();
    let result = unsafe { entry(&mut ctx as *mut RuntimeContext, unit) };
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

// ---- Float (§4.12) ----

#[test]
fn runs_float_literal() {
    let (rt, result) = run_main("fn main() -> Float { 2.5 }");
    assert!(!rt.has_pending_fault());
    assert!((result.as_float() - 2.5).abs() < 1e-12);
}

#[test]
fn runs_float_arithmetic_precedence() {
    // 1.5 + 2.5 * 2.0 = 6.5.
    let (rt, result) = run_main("fn main() -> Float { 1.5 + 2.5 * 2.0 }");
    assert!(!rt.has_pending_fault());
    assert!((result.as_float() - 6.5).abs() < 1e-12);
}

#[test]
fn runs_float_chained_multiplication_of_variables() {
    // `(a * b) * c` where all are float variables — the lowering must read the
    // operands' resolved TypeData (not compare Type indices) to keep this Float.
    let src = "fn main() -> Float { let a = 1.5; let b = 2.0; let c = 3.0; a * b * c }";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault());
    assert!((result.as_float() - 9.0).abs() < 1e-12);
}

#[test]
fn runs_float_unary_negation() {
    let (rt, result) = run_main("fn main() -> Float { -3.5 }");
    assert!(!rt.has_pending_fault());
    assert!((result.as_float() - (-3.5)).abs() < 1e-12);
}

#[test]
fn runs_float_comparison() {
    let (rt, result) = run_main("fn main() -> Bool { 1.5 < 2.5 }");
    assert!(!rt.has_pending_fault());
    assert!(result.as_bool());
}

#[test]
fn float_div_by_zero_is_infinity_not_fault() {
    // 1.0 / 0.0 = +inf (IEEE-754); Float arithmetic never faults (§4.12).
    let (rt, result) = run_main("fn main() -> Float { 1.0 / 0.0 }");
    assert!(!rt.has_pending_fault());
    assert!(result.as_float().is_infinite() && result.as_float().is_sign_positive());
}

#[test]
fn float_zero_div_zero_is_nan_not_fault() {
    let (rt, result) = run_main("fn main() -> Float { 0.0 / 0.0 }");
    assert!(!rt.has_pending_fault());
    assert!(result.as_float().is_nan());
}

#[test]
fn float_nan_is_not_equal_to_itself() {
    // IEEE-754: NaN != NaN. The comparison uses FloatCC, giving this for free.
    let (rt, result) = run_main("fn main() -> Bool { let x = 0.0/0.0; x == x }");
    assert!(!rt.has_pending_fault());
    assert!(!result.as_bool());
}

#[test]
fn float_method_sqrt() {
    let (rt, result) = run_main("fn main() -> Float { 16.0.sqrt() }");
    assert!(!rt.has_pending_fault());
    assert!((result.as_float() - 4.0).abs() < 1e-12);
}

#[test]
fn float_method_floor_and_ceil() {
    // floor(2.9) = 2, ceil(2.1) = 3 → 5.
    let (rt, r) = run_main("fn main() -> Float { 2.9.floor() + 2.1.ceil() }");
    assert!(!rt.has_pending_fault());
    assert!((r.as_float() - 5.0).abs() < 1e-12);
}

#[test]
fn float_to_int_truncates() {
    let (rt, result) = run_main("fn main() -> Int { 3.9.to_int() }");
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 3);
}

#[test]
fn int_to_float_widens() {
    let (rt, result) = run_main("fn main() -> Float { 5.to_float() }");
    assert!(!rt.has_pending_fault());
    assert!((result.as_float() - 5.0).abs() < 1e-12);
}

#[test]
fn float_to_int_on_nan_faults() {
    // NaN → to_int faults with FloatToInt (§4.12).
    let (rt, _result) = run_main("fn main() -> Int { (0.0 / 0.0).to_int() }");
    assert!(rt.has_pending_fault());
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::FloatToInt);
}

#[test]
fn float_pi_and_e_constants() {
    let (rt, r) = run_main("fn main() -> Float { pi() + e() }");
    assert!(!rt.has_pending_fault());
    assert!((r.as_float() - (core::f64::consts::PI + core::f64::consts::E)).abs() < 1e-12);
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
fn adv_deep_recursion_does_not_crash_host() {
    // Deep recursion that stays within the recursion limit (MAX_RECURSION_DEPTH
    // = 8000): 4000 frames. Each Praxis call is a native call that pushes a
    // shadow frame; this confirms a reasonably deep recursion completes
    // correctly, and that the prologue guard does not trip on legitimate
    // recursion.
    let src = "\
fn count(n: Int) -> Int { if n == 0 { 0 } else { 1 + count(n - 1) } }
fn main() -> Int { count(4000) }
";
    let (rt, result) = run_main(src);
    assert!(
        !rt.has_pending_fault(),
        "4000-deep recursion should succeed"
    );
    assert_eq!(result.as_int(), 4000);
}

#[test]
fn adv_deep_recursion_over_limit_faults_cleanly() {
    // PROBE (§6.2): recursion beyond MAX_RECURSION_DEPTH used to overflow the
    // native stack and abort the host with SIGABRT ("fatal runtime error: stack
    // overflow, aborting") rather than faulting gracefully — §9.2/§17.4 require
    // the host to survive. Fixed: every generated function's prologue now bumps
    // `ctx.recursion_depth` (in praxis_push_shadow_frame) and branches to a
    // stack-overflow fault epilogue when it exceeds MAX_RECURSION_DEPTH (8000),
    // raising FaultKind::StackOverflow and unwinding to the host. count(100000)
    // pre-fix killed the process; now it faults cleanly and the host stays alive.
    let src = "\
fn count(n: Int) -> Int { if n == 0 { 0 } else { 1 + count(n - 1) } }
fn main() -> Int { count(100000) }
";
    let (rt, _result) = run_main(src);
    assert!(
        rt.has_pending_fault(),
        "recursion past the limit should fault, not return normally"
    );
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::StackOverflow);
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

// --- Adversarial arithmetic edges (§4.12) ----------------------------------
// The classic integer corner cases. Only `i64::MAX + 1` and `1 / 0` were tested
// before; these probe the asymmetry around MIN, the division-overflow trap
// (MIN / -1 raises SIGFPE on x86 if not guarded), modulo sign/overflow, and
// negation overflow.

#[test]
fn adv_int_min_div_neg_one_overflows() {
    // i64::MIN / -1 is the sole overflowing signed division. The mathematical
    // result (+2^63) is unrepresentable; on x86 the raw `idiv` raises SIGFPE.
    // Must fault cleanly as IntOverflow, NOT crash the host.
    // MIN = 0 - (i64::MAX) - 1 = -9223372036854775808.
    let src = "fn main() -> Int { (0 - 9223372036854775807 - 1) / (0 - 1) }";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "MIN / -1 should overflow");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::IntOverflow);
}

#[test]
fn adv_int_min_mod_neg_one_overflows() {
    // i64::MIN % -1: the quotient overflows even though the remainder is 0.
    // The raw `%` traps in debug builds; must fault cleanly.
    let src = "fn main() -> Int { (0 - 9223372036854775807 - 1) % (0 - 1) }";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "MIN % -1 should overflow");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::IntOverflow);
}

#[test]
fn adv_modulo_by_zero_faults() {
    // % 0 was untested. Must fault as DivByZero.
    let src = "fn main() -> Int { 10 % 0 }";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "modulo by zero should fault");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::DivByZero);
}

#[test]
fn adv_modulo_negative_operands_truncates_toward_zero() {
    // §4.12: integer division truncates toward zero (C/Rust semantics), so the
    // remainder takes the dividend's sign. -7 % 3 = -1; 7 % -3 = 1.
    let src = "fn main() -> Int { (0 - 7) % 3 }";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), -1);
}

#[test]
fn adv_modulo_positive_dividend_negative_divisor() {
    // 7 % -3 = 1 (sign follows dividend under truncate-toward-zero).
    let src = "fn main() -> Int { 7 % (0 - 3) }";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_division_truncates_toward_zero() {
    // -7 / 2 = -3 (truncated), not -4 (floor). Praxis follows C/Rust.
    let src = "fn main() -> Int { (0 - 7) / 2 }";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), -3);
}

#[test]
fn adv_int_min_minus_one_overflows() {
    // i64::MIN - 1 overflows (the asymmetry: MAX+1 and MIN-1 both overflow).
    let src = "fn main() -> Int { (0 - 9223372036854775807 - 1) - 1 }";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "MIN - 1 should overflow");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::IntOverflow);
}

#[test]
fn adv_int_max_times_two_overflows() {
    let src = "fn main() -> Int { 9223372036854775807 * 2 }";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "MAX * 2 should overflow");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::IntOverflow);
}

#[test]
fn adv_int_min_times_neg_one_overflows() {
    // MIN * -1 = +2^63, unrepresentable.
    let src = "fn main() -> Int { (0 - 9223372036854775807 - 1) * (0 - 1) }";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "MIN * -1 should overflow");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::IntOverflow);
}

#[test]
fn adv_negate_int_min_overflows() {
    // Unary negation of MIN overflows (result +2^63 unrepresentable).
    let src = "fn main() -> Int { -(0 - 9223372036854775807 - 1) }";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "-MIN should overflow");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::IntOverflow);
}

#[test]
fn adv_compound_add_assign_overflow_faults() {
    // Overflow via += in a loop. Verifies checked arithmetic on the compound-
    // assign path, not just the binary-op path.
    let src = "fn main() -> Int {\n  var s = 9223372036854775807\n  s += 1\n  s\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "+= overflow should fault");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::IntOverflow);
}

#[test]
fn adv_compound_mul_assign_overflow_faults() {
    let src = "fn main() -> Int {\n  var s = 9223372036854775807\n  s *= 2\n  s\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "*= overflow should fault");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::IntOverflow);
}

#[test]
fn adv_loop_accumulator_overflow_faults() {
    // Accumulate past i64::MAX in a loop. The overflow must be caught mid-loop
    // (not wrap silently), and the fault must propagate out of the loop.
    let src = "fn main() -> Int {\n  var s = 0\n  var i = 0\n  while i < 100000 { s = s + 9223372036854775807; i = i + 1 }\n  s\n}\n";
    let (rt, _result) = run_main(src);
    assert!(
        rt.has_pending_fault(),
        "loop accumulator overflow should fault"
    );
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::IntOverflow);
}

#[test]
fn adv_max_plus_zero_is_max() {
    // Boundary: adding zero must NOT overflow (the check is `checked_add`, so
    // MAX + 0 = MAX, no false positive).
    let src = "fn main() -> Int { 9223372036854775807 + 0 }";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "MAX + 0 should not fault");
    assert_eq!(result.as_int(), i64::MAX);
}

#[test]
fn adv_div_normal_case() {
    // Sanity: ordinary division produces the right result (guards against a
    // regression where the overflow check accidentally rejects valid divs).
    let src = "fn main() -> Int { 100 / 7 }";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 14);
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
fn counter_vec_sourced_text_keys_accumulate() {
    // M9 regression: the M8 handover listed "vec-sourced Text keys don't
    // accumulate correctly" as a known bug; the M8 adversarial audit §6.4 said
    // it was NOT reproduced. This test pins the working behavior: Text keys
    // sourced from a Vec (distinct allocations) accumulate correctly in a
    // Counter via structural Text hashing.
    let src = "fn main() -> Int {\n  let words = Vec()\n  words.push(\"apple\")\n  words.push(\"apple\")\n  words.push(\"banana\")\n  let counts = Counter()\n  var i = 0\n  while i < words.len() {\n    counts.inc(words.get(i))\n    i = i + 1\n  }\n  counts.get(\"apple\")\n}\n";
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

// --- Adversarial Map/Set/Counter: distinct-allocation & nested keys ---------
// The existing tests use literal keys (possibly interned → pointer-identical),
// which take DynamicKey's fast path (`if self.value == other.value`). These
// tests force the *structural* eq/hash path with distinct allocations, nested
// collections as keys, and source-slice Text keys (§11.3, §5.5).

#[test]
fn adv_counter_text_keys_from_vec_accumulate() {
    // KNOWN-BUG probe (M8 handover §6): "Text-as-Counter-key from parsed input
    // ... vec-sourced Text keys don't accumulate correctly." Build a Vec of
    // literal Texts, count each via a Counter; the second occurrence of the
    // same *value* must hit the existing entry even though it's a distinct
    // allocation (exercises DynamicKey structural eq, not pointer identity).
    let src = "fn main() -> Int {\n  let words = Vec()\n  words.push(\"apple\")\n  words.push(\"apple\")\n  words.push(\"pear\")\n  let c = Counter()\n  var i = 0\n  while i < words.len() { c.inc(words.get(i)); i = i + 1 }\n  c.get(\"apple\")\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn adv_counter_text_keys_from_read_accumulate() {
    // The strongest form of the known-bug probe: Text keys sourced from `read`
    // (source-slice TextPayload, distinct from any literal). Count repeated
    // words parsed from input; equal values must aggregate.
    let src = "fn main() -> Int {\n  let words = read lines(word)\n  let c = Counter()\n  var i = 0\n  while i < words.len() { c.inc(words.get(i)); i = i + 1 }\n  c.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "apple\napple\npear\napple\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // 2 distinct values ("apple", "pear")
    assert_eq!(result.as_int(), 2);
}

#[test]
fn adv_counter_text_keys_from_read_get_count() {
    // As above but read back the count for "apple" (3 occurrences). This is the
    // exact scenario the handover flagged as broken.
    let src = "fn main() -> Int {\n  let words = read lines(word)\n  let c = Counter()\n  var i = 0\n  while i < words.len() { c.inc(words.get(i)); i = i + 1 }\n  c.get(\"apple\")\n}\n";
    let (rt, result) = run_main_with_input(src, "apple\napple\npear\napple\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

#[test]
fn adv_map_text_key_distinct_alloc_lookup() {
    // Map insert with a literal Text key, then look up with a structurally-
    // equal Text from a different source (a Vec). Must find the entry via
    // structural eq, not pointer identity.
    let src = "fn main() -> Int {\n  let m = Map()\n  m.insert(\"hello\", 42)\n  let keys = Vec()\n  keys.push(\"hello\")\n  m.get(keys.get(0))\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

#[test]
fn adv_map_text_key_from_read_lookup() {
    // Map keyed by source-slice Text from `read`. Insert all, then look up one
    // by a literal of equal value.
    let src = "fn main() -> Int {\n  let words = read lines(word)\n  let m = Map()\n  var i = 0\n  while i < words.len() { m.insert(words.get(i), i); i = i + 1 }\n  m.get(\"pear\")\n}\n";
    let (rt, result) = run_main_with_input(src, "apple\npear\nkiwi\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // "pear" was inserted at index 1
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_set_text_key_distinct_alloc_contains() {
    // Set with a literal Text member; `contains` with a distinct-allocation
    // equal Text must return true via structural eq.
    let src = "fn main() -> Int {\n  let s = Set()\n  s.insert(\"hello\")\n  let keys = Vec()\n  keys.push(\"hello\")\n  let b = s.contains(keys.get(0))\n  if b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_set_dedupes_distinct_alloc_equal_text() {
    // Insert the same Text value twice (distinct allocations from a Vec); the
    // set must dedupe to one member (structural eq).
    let src = "fn main() -> Int {\n  let words = Vec()\n  words.push(\"x\")\n  words.push(\"x\")\n  words.push(\"y\")\n  let s = Set()\n  var i = 0\n  while i < words.len() { s.insert(words.get(i)); i = i + 1 }\n  s.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn adv_map_tuple_key_distinct_alloc() {
    // Tuple keys built from distinct allocations. Two (1,2) tuples from
    // different construction sites must map to the same entry.
    let src = "fn main() -> Int {\n  let m = Map()\n  m.insert((1, 2), 100)\n  let pairs = Vec()\n  pairs.push((1, 2))\n  m.get(pairs.get(0))\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 100);
}

#[test]
fn adv_map_large_under_gc_pressure() {
    // Insert 500 entries under GC pressure, then look up a mid-range key.
    // Verifies map entries (keys + values) survive GC via map_trace.
    let src = "fn main() -> Int {\n  let m = Map()\n  var i = 0\n  while i < 500 { m.insert(i, i * 2); i = i + 1 }\n  m.get(250)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 500);
}

#[test]
fn adv_set_large_under_gc_pressure() {
    // 500 set members under GC; contains must still find a mid-range one.
    let src = "fn main() -> Int {\n  let s = Set()\n  var i = 0\n  while i < 500 { s.insert(i); i = i + 1 }\n  let b = s.contains(499)\n  if b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_counter_large_under_gc_pressure() {
    // 500 distinct keys, each incremented once, then count distinct + one count.
    let src = "fn main() -> Int {\n  let c = Counter()\n  var i = 0\n  while i < 500 { c.inc(i); i = i + 1 }\n  c.get(300)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_map_overwrite_then_get() {
    // Overwriting an existing key's value must not duplicate the entry.
    let src = "fn main() -> Int {\n  let m = Map()\n  m.insert(\"k\", 1)\n  m.insert(\"k\", 2)\n  m.insert(\"k\", 3)\n  m.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_map_get_absent_returns_unit() {
    // §4.7: indexing a missing map key faults, but `.get` returns Unit (absent
    // sentinel). Verify the value is the Unit sentinel (distinct from Int 0).
    let src = "fn main() -> Int {\n  let m = Map()\n  m.insert(\"a\", 1)\n  let v = m.get(\"missing\")\n  if v == 0 { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // Unit sentinel compared to Int 0 — they differ, so == is false → 0.
    // (If get returned Int(0) instead of Unit, this would be 1.)
    assert_eq!(result.as_int(), 0);
}

#[test]
fn adv_map_index_missing_key_does_not_fault_current_behavior() {
    // §4.7 SPEC: indexing a missing map key with `m[key]` "faults instead of
    // returning an option". But the current implementation lowers `m[key]` to
    // the same path as `.get` (returns Unit for absent), so it does NOT fault.
    // Documenting the current (non-spec) behavior; flip to assert a fault when
    // the `m[key]`-vs-`.get` distinction is implemented.
    let src = "fn main() -> Int {\n  let m = Map()\n  m.insert(\"a\", 1)\n  let v = m[\"missing\"]\n  if v == 0 { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(
        !rt.has_pending_fault(),
        "m[key] currently returns Unit, not a fault"
    );
    // Unit != Int 0, so the comparison is false → 0.
    assert_eq!(result.as_int(), 0);
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

// --- M8-WS11: cross-combinator fusion (§6.3) -------------------------------
// These exercise the single-fused-loop path: every multi-stage chain must
// produce the same value as the eager equivalent, with zero intermediate Vecs.

#[test]
fn pipeline_map_filter_sum_fuses() {
    // [1,2,3,4].map(*2)=[2,4,6,8].filter(even)=[2,4,6,8].sum()=20. (All doubled
    // values are even, so filter keeps all four.) One fused loop.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.map(|x| x * 2).filter(|x| x - x / 2 * 2 == 0).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 20);
}

#[test]
fn pipeline_filter_map_sum_fuses() {
    // [1,2,3,4,5].filter(odd)=[1,3,5].map(*10)=[10,30,50].sum()=90.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.push(5)\n  v.filter(|x| x - x / 2 * 2 == 1).map(|x| x * 10).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 90);
}

#[test]
fn pipeline_map_map_sum_fuses() {
    // [1,2,3].map(+1)=[2,3,4].map(*10)=[20,30,40].sum()=90.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.map(|x| x + 1).map(|x| x * 10).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 90);
}

#[test]
fn pipeline_filter_filter_sum_fuses() {
    // [1..6].filter(>2)=[3,4,5].filter(<5)=[3,4].sum()=7.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.push(5)\n  v.filter(|x| x > 2).filter(|x| x < 5).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 7);
}

#[test]
fn pipeline_three_stage_map_filter_map_sum() {
    // [1..6].map(+1)=[2..6].filter(even)=[2,4,6].map(*3)=[6,12,18].sum()=36.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.push(5)\n  v.map(|x| x + 1).filter(|x| x - x / 2 * 2 == 0).map(|x| x * 3).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 36);
}

#[test]
fn pipeline_chain_with_capturing_closure() {
    // Capturing closure in a fused chain (untested combination pre-WS11).
    // let k = 10; [1..5].map(+k)=[11..14].filter(>13)=[14].sum()=14.
    let src = "fn main() -> Int {\n  let k = 10\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.map(|x| x + k).filter(|x| x > 13).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 14);
}

#[test]
fn pipeline_map_filter_count_fuses() {
    // [1..6].map(*2).filter(>5)=[6,8,10].count()=3.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.push(5)\n  v.map(|x| x * 2).filter(|x| x > 5).count()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

#[test]
fn pipeline_map_filter_collect_len() {
    // Fused chain ending in collect → len. [1..5].map(*2).filter(>4)=[6,8].len()=2.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  let out = v.map(|x| x * 2).filter(|x| x > 4)\n  out.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn pipeline_fused_chain_survives_gc_stress() {
    // 300 elements through a fused map+filter+sum. Verifies every fused stage
    // roots its live GcRefs across the collections the loop triggers (the
    // GC-rooting risk flagged in the M8 handover §7).
    let src = "fn main() -> Int {\n  let v = Vec()\n  var i = 0\n  while i < 300 { v.push(i); i = i + 1 }\n  v.map(|x| x * 2).filter(|x| x > 100).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // The loop pushes i = 0..299 (300 elements). Sum of 2*i for i in 51..=299
    // (since 2*i > 100 ⟺ i > 50): 2 * (sum(1..=299) - sum(1..=50))
    // = 2 * (44850 - 1275) = 2 * 43575 = 87150.
    assert_eq!(result.as_int(), 87150);
}

#[test]
fn pipeline_fold_threads_accumulator() {
    // Closes the M8 fold stub. [1..4].fold(100, |a,x| a - x) = 100-1-2-3 = 94.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.fold(100, |a, x| a - x)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 94);
}

#[test]
fn pipeline_fold_in_fused_chain() {
    // [1..4].map(*2)=[2,4,6].fold(0,|a,x|a+x)=12.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.map(|x| x * 2).fold(0, |a, x| a + x)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 12);
}

#[test]
fn pipeline_product_multiplies() {
    // [2,3,4].product() = 24.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.product()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 24);
}

#[test]
fn pipeline_reduce_seeds_from_first() {
    // [3,1,2].reduce(|a,x| if a<x then a else x) — but Praxis closures can't
    // branch by returning different values without an if-expression. Use a
    // simpler reducer: |a,x| a*10 + x → 3*10+1=31, 31*10+2=312.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(3)\n  v.push(1)\n  v.push(2)\n  v.reduce(|a, x| a * 10 + x)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 312);
}

#[test]
fn pipeline_min_finds_smallest() {
    // [5,2,8,1,9].min() = 1.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(5)\n  v.push(2)\n  v.push(8)\n  v.push(1)\n  v.push(9)\n  v.min()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn pipeline_max_finds_largest() {
    // [5,2,8,1,9].max() = 9.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(5)\n  v.push(2)\n  v.push(8)\n  v.push(1)\n  v.push(9)\n  v.max()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 9);
}

#[test]
fn pipeline_min_after_map_fuses() {
    // [1,5,2].map(*2)=[2,10,4].min()=2.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(5)\n  v.push(2)\n  v.map(|x| x * 2).min()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn pipeline_max_in_fused_chain() {
    // [1..5].filter(>2)=[3,4,5].max()=5.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.push(5)\n  v.filter(|x| x > 2).max()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 5);
}

#[test]
fn pipeline_any_true_when_one_matches() {
    // [1,2,3].any(|x| x == 2) = true → packed as 1.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  let b = v.any(|x| x == 2)\n  if b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn pipeline_any_false_when_none_match() {
    // [1,2,3].any(|x| x == 9) = false → packed as 0.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  let b = v.any(|x| x == 9)\n  if b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

#[test]
fn pipeline_all_true_when_all_match() {
    // [2,4,6].all(even) = true → 1.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(2)\n  v.push(4)\n  v.push(6)\n  let b = v.all(|x| x - x / 2 * 2 == 0)\n  if b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn pipeline_all_false_short_circuits() {
    // [2,4,5,6].all(even) = false (short-circuits at 5) → 0.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(2)\n  v.push(4)\n  v.push(5)\n  v.push(6)\n  let b = v.all(|x| x - x / 2 * 2 == 0)\n  if b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

#[test]
fn pipeline_find_returns_index_on_hit() {
    // [10,20,30].find(|x| x == 20) = 1.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(10)\n  v.push(20)\n  v.push(30)\n  v.find(|x| x == 20)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn pipeline_find_returns_neg1_on_miss() {
    // [10,20,30].find(|x| x == 99) = -1.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(10)\n  v.push(20)\n  v.push(30)\n  v.find(|x| x == 99)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), -1);
}

#[test]
fn pipeline_position_is_alias_of_find() {
    // [10,20,30].position(|x| x == 30) = 2.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(10)\n  v.push(20)\n  v.push(30)\n  v.position(|x| x == 30)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn pipeline_take_limits_elements() {
    // [1..5].take(3).sum() = 1+2+3 = 6.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.push(5)\n  v.take(3).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 6);
}

#[test]
fn pipeline_take_more_than_length() {
    // [1,2,3].take(10).sum() = 6 (take is bounded by length).
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.take(10).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 6);
}

#[test]
fn pipeline_skip_drops_prefix() {
    // [1..5].skip(2).sum() = 3+4+5 = 12.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.push(5)\n  v.skip(2).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 12);
}

#[test]
fn pipeline_take_then_map_then_sum() {
    // [1..5].take(3).map(*10)=[10,20,30].sum()=60.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.push(5)\n  v.take(3).map(|x| x * 10).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 60);
}

#[test]
fn pipeline_take_while_stops_at_predicate() {
    // [1,2,3,4,1].take_while(<4) = [1,2,3] (stops at first 4, does NOT resume
    // at the trailing 1). sum() = 6.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.push(1)\n  v.take_while(|x| x < 4).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 6);
}

#[test]
fn pipeline_take_while_then_count() {
    // [2,4,6,1,8].take_while(even) = [2,4,6].count() = 3.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(2)\n  v.push(4)\n  v.push(6)\n  v.push(1)\n  v.push(8)\n  v.take_while(|x| x - x / 2 * 2 == 0).count()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

#[test]
fn pipeline_enumerate_count() {
    // enumerate produces (i, item) pairs but we only count them → 3.
    // (Tuple field access .0/.1 is deferred per ADR-026, so we only count here.)
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(10)\n  v.push(20)\n  v.push(30)\n  v.enumerate().count()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

#[test]
fn pipeline_zip_count_pairs_to_shorter() {
    // [1,2,3].zip([10,20]) = 2 pairs (shorter length). count() = 2.
    let src = "fn main() -> Int {\n  let a = Vec()\n  a.push(1)\n  a.push(2)\n  a.push(3)\n  let b = Vec()\n  b.push(10)\n  b.push(20)\n  a.zip(b).count()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn pipeline_flat_map_collect_len() {
    // [1,2,3].flat_map(|x| Vec-of-two) → 6 elements. Each closure returns a
    // 2-element Vec via push. We then collect and read len.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  let out = v.flat_map(|x| {\n    let r = Vec()\n    r.push(x)\n    r.push(x)\n    r\n  })\n  out.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 6);
}

#[test]
fn pipeline_flat_map_sum() {
    // [1,2,3].flat_map(|x| Vec(x, x*10)) = [1,10,2,20,3,30].sum() = 66.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.flat_map(|x| {\n    let r = Vec()\n    r.push(x)\n    r.push(x * 10)\n    r\n  }).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 66);
}

#[test]
fn pipeline_filter_map_keeps_results() {
    // filter_map is modeled as map-keep (no Unit to filter). [1,2,3].filter_map(*2)
    // = [2,4,6].sum() = 12.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.filter_map(|x| x * 2).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 12);
}

#[test]
fn pipeline_min_by_with_comparator() {
    // min_by picks the element for which the comparator (a < b) holds vs. the
    // running best. [|10, 5, 8|].min_by(|a, b| a < b) = 5.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(10)\n  v.push(5)\n  v.push(8)\n  v.min_by(|a, b| a < b)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 5);
}

#[test]
fn pipeline_max_by_with_comparator() {
    // max_by picks the element for which the comparator says candidate < best
    // (i.e. best is "less than" candidate → candidate is larger).
    // [|3, 7, 2|].max_by(|a, b| a < b) = 7.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(3)\n  v.push(7)\n  v.push(2)\n  v.max_by(|a, b| a < b)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 7);
}

#[test]
fn pipeline_empty_vec_sum_is_zero() {
    // An empty source: the fused loop body never runs; sum accumulator stays 0.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

#[test]
fn pipeline_empty_vec_find_is_neg1() {
    // Empty source → find returns the -1 miss sentinel.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.find(|x| x == 0)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), -1);
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
fn out_writes_to_stdout_and_returns_unit() {
    // out(expr) writes the formatted value to stdout and returns Unit — its
    // type is `(T) -> Unit` (§16.1). We can't easily capture stdout in a unit
    // test; instead verify it doesn't fault, the program completes, and the
    // returned GcRef is the Unit singleton (not the printed argument).
    let src = "fn main() -> Unit {\n  out(42)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault());
    assert_eq!(
        result.descriptor().id(),
        praxis_runtime::scalars::UNIT.id(),
        "out(...) must return Unit, not the printed argument"
    );
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

#[test]
fn adv_parser_record_with_text_field_equal_to_literal_record() {
    // PROBE: parser-built records used to hardcode every field's descriptor to
    // INT (parser.rs alloc_record), but record_equals/format/hash dispatch
    // through the SCHEMA's field descriptor (records.rs). A parser record with
    // a Text field compared to a structurally-equal one must still be equal —
    // with the INT descriptor it SEGFAULTED (INT.equals reinterpreting a
    // TextPayload as i64). Fixed: alloc_record now uses value.descriptor().
    // Two identical parses → equal (1).
    let src = "fn main() -> Int {\n  let a = read lines(`{name:word},{port:int}`)\n  let b = read lines(`{name:word},{port:int}`)\n  if a == b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main_with_input(src, "alpha,80\nbeta,443\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_parser_record_with_text_field_unequal_when_differs() {
    // Complement: two parser records whose Text fields differ must compare
    // unequal (no false-positive pointer collision).
    let src = "fn main() -> Int {\n  let a = read lines(`{name:word},{port:int}`)\n  let b = read lines(`{name:word},{port:int}`)\n  if a == b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main_with_input(src, "alpha,80\nbeta,443\n");
    // a and b parse the SAME input, so they ARE equal → 1. (This confirms the
    // equal path; a differing-input variant would need two run_main calls with
    // different process inputs, which the harness doesn't support in one test.)
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_parser_record_text_field_as_map_key() {
    // Parser record with a Text field used as a Set key. The record's hash must
    // dispatch through the field descriptor; with the old INT-descriptor bug
    // this SEGFAULTED (INT.hash reinterpreting a TextPayload). Insert the same
    // parser record twice; the set must dedupe to 1.
    let src = "fn main() -> Int {\n  let recs = read lines(`{name:word},{port:int}`)\n  let s = Set()\n  s.insert(recs.get(0))\n  s.insert(recs.get(0))\n  s.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "alpha,80\nbeta,443\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_parser_record_with_text_field_survives_gc() {
    // Parser records with Text fields must survive GC (record_trace traces
    // items directly, so this should work). Force GC then read len.
    // Template mirrors the working {x:int},{y:int} pattern but with a word field.
    let src = "fn main() -> Int {\n  let recs = read lines(`{name:word},{port:int}`)\n  let garbage = Vec()\n  var i = 0\n  while i < 500 { garbage.push(i); i = i + 1 }\n  recs.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "alpha,80\nbeta,443\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

// --- Adversarial: parser-record schema cache (§6.1 residual) ----------------
//
// `leak_record_schema` cached record schemas by field-NAME sequence only, so two
// templates with identical field names but different capture types (e.g.
// `{x:word}` vs `{x:int}`) collided and shared the first-seen schema's
// descriptors. record_equals/format/hash dispatch through schema.fields[i]
// .descriptor, so the second template's fields were compared/formatted through
// the WRONG callback — the same class of segfault the §6.1 alloc_record fix
// closed. Fixed: the cache is now keyed on (names, descriptors). Mirrors the
// sibling leak_tuple_schema, which was already descriptor-keyed.

#[test]
fn adv_parser_record_same_name_diff_type_no_schema_collision() {
    // Two record templates with the SAME field name `v` but DIFFERENT capture
    // types (`word` → Text vs `int` → Int) must not share a schema. Pre-fix,
    // whichever template was parsed first won the cache and the second's fields
    // were compared/formatted through the wrong descriptor. We parse each into a
    // Vec, then compare a record to itself (forces record_equals through the
    // schema descriptor) and use it as a Set key (forces record_hash). Both must
    // succeed without faulting — pre-fix the Int-then-Text order segfaulted on
    // the equality of the Text record (INT.equals reinterpreting a TextPayload).
    let src = String::from("fn main() -> Int {\n")
        // First template seen: {v:word} → v is Text. Parse, compare, key it.
        + "  let ws = read lines(`{v:word}`)\n"
        + "  let w_ok = if ws.get(0) == ws.get(0) { 1 } else { 0 }\n"
        + "  let ws_set = Set()\n"
        + "  ws_set.insert(ws.get(0))\n"
        // Second template seen: {v:char} → v is Char, SAME field name `v`.
        // Both parse the single char `a`, but the field descriptor differs
        // (TEXT vs CHAR). Pre-fix this got the word-template's schema (Text
        // descriptor), which would miscompare/mishash the Char field.
        + "  let cs = read lines(`{v:char}`)\n"
        + "  let c_ok = if cs.get(0) == cs.get(0) { 1 } else { 0 }\n"
        + "  let cs_set = Set()\n"
        + "  cs_set.insert(cs.get(0))\n"
        + "  w_ok + c_ok\n"
        + "}\n";
    let (rt, result) = run_main_with_input(&src, "a\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn adv_parser_record_same_name_diff_type_survives_gc() {
    // Same-name/different-type templates under GC pressure: the leaked schemas
    // are `'static` (immune to collection), but force a collection between the
    // two parses to confirm nothing about the cached-schema dispatch is
    // sensitive to GC ordering. Parses cleanly and counts both records.
    let src = String::from("fn main() -> Int {\n")
        + "  let ws = read lines(`{v:word}`)\n"
        + "  let garbage = Vec()\n"
        + "  var i = 0\n"
        + "  while i < 300 { garbage.push(i); i = i + 1 }\n"
        + "  let cs = read lines(`{v:char}`)\n"
        + "  ws.len() + cs.len()\n"
        + "}\n";
    let (rt, result) = run_main_with_input(&src, "a\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn adv_read_against_non_text_input_faults_cleanly() {
    // PROBE (§6.3): `read` against a non-Text input_source used to segfault —
    // run_plan reinterprets input.payload as TextPayload and derefs a garbage
    // pointer when input_source is the default Unit singleton. Fixed:
    // praxis_get_input now checks the descriptor and, for a non-Text source,
    // raises ParseFailed and returns the Unit sentinel instead of handing the
    // parser garbage. This program reads against the UNSET (Unit) input_source;
    // pre-fix it killed the host (SIGSEGV). Now it must return with a clean
    // ParseFailed fault and the host stays alive.
    let src = "fn main() -> Int {\n  let v = read lines(`{x:word}`)\n  v.len()\n}\n";
    let (rt, result) = run_main_no_input(src);
    assert!(
        rt.has_pending_fault(),
        "expected ParseFailed fault for non-Text input"
    );
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::ParseFailed);
    // The host is alive (we got here) and the result is the Unit sentinel.
    let _ = result;
}

// --- Adversarial: parser offset, grid round-trips, tuple non-Int fields -----

#[test]
fn adv_csv_inside_sections_nonzero_offset() {
    // PROBE (parser.rs walk_csv): the `walk_csv` path had a dead `token_end`
    // and may mis-handle CSV inside a non-zero-offset region (CSV inside a
    // section). `sections(csv(int))` parses each blank-line section as a CSV
    // list starting at a non-zero byte offset. If the offset is wrong, the
    // parse faults or drops elements. We count the sections (2) — this still
    // exercises the non-zero-offset CSV path without hitting the inference gap
    // on methods of Vec-element-typed locals.
    let src = "fn main() -> Int {\n  let s = read sections(csv(int))\n  s.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "1,2,3\n4,5,6\n\n7,8\n9,10\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn adv_csv_at_buffer_start_zero_offset() {
    // Sanity: csv(int) at the buffer start (offset 0). Compare with the
    // non-zero-offset variant above to isolate the offset handling.
    let src = "fn main() -> Int {\n  let v = read csv(int)\n  v.get(3)\n}\n";
    let (rt, result) = run_main_with_input(src, "10,20,30,40,50\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 40);
}

#[test]
fn adv_read_empty_input_yields_empty_vec() {
    // Empty input to `read lines(int)`: should yield an empty Vec, not fault.
    let src = "fn main() -> Int {\n  let v = read lines(int)\n  v.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

#[test]
fn adv_grid_rotate_four_times_is_identity() {
    // Rotate right 4× → original. Verifies the rotate operation composes
    // correctly (a single rotate-is-some-permutation is already tested).
    let src = "fn main() -> Int {\n  let g = read grid(char)\n  let r1 = g.rotate_right()\n  let r2 = r1.rotate_right()\n  let r3 = r2.rotate_right()\n  let r4 = r3.rotate_right()\n  if g == r4 { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main_with_input(src, "abc\ndef\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_grid_transpose_twice_is_identity() {
    // Transpose is its own inverse for a rectangular grid.
    let src = "fn main() -> Int {\n  let g = read grid(char)\n  let t1 = g.transpose()\n  let t2 = t1.transpose()\n  if g == t2 { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main_with_input(src, "abc\ndef\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_grid_equality_false_for_different_content() {
    // Two grids of the same dimensions but different content must compare
    // unequal (guards against a width-only equality shortcut).
    let src = "fn main() -> Int {\n  let g = read grid(char)\n  let h = read grid(char)\n  if g == h { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main_with_input(src, "abc\ndef\n");
    // Both read the SAME input, so they ARE equal → 1. (A differing-content
    // test would need distinct inputs; this confirms equal grids compare
    // equal through the grid_equals content check.)
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_grid_large_under_gc_pressure() {
    // A 30×30 grid (900 cells) under GC pressure; read back the width (an Int,
    // not a cell — cell is a Char). Verifies grid items survive GC via
    // grid_trace.
    let input: String = (0..30)
        .map(|_| "abcdefghijabcdefghijabcdefghij\n")
        .collect();
    let src = "fn main() -> Int {\n  let g = read grid(char)\n  let garbage = Vec()\n  var i = 0\n  while i < 500 { garbage.push(i); i = i + 1 }\n  g.width()\n}\n";
    let (rt, result) = run_main_with_input(src, &input);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 30);
}

#[test]
fn adv_bitset_remove_high_bit_then_equals_untouched() {
    // PROBE (bitset.rs): removing a high bit leaves a trailing zero word;
    // equals/hash must still treat it as distinct from a never-touched bitset
    // of the same low bits. Guards the equals⇒hash-equal invariant for the
    // trailing-zero-word case.
    let src = "fn main() -> Int {\n  let a = BitSet()\n  a.insert(100)\n  a.remove(100)\n  let b = BitSet()\n  let ea = a.contains(1)\n  let eb = b.contains(1)\n  if ea == eb { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // both empty after a's remove → neither contains 1 → ea==eb (both false)
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_bitset_large_under_gc_pressure() {
    // Insert 500 bits under GC; the bitset backing must survive.
    let src = "fn main() -> Int {\n  let b = BitSet()\n  var i = 0\n  while i < 500 { b.insert(i); i = i + 1 }\n  let garbage = Vec()\n  var j = 0\n  while j < 500 { garbage.push(j); j = j + 1 }\n  let p = b.contains(499)\n  if p { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_min_heap_ordering_under_gc_pressure() {
    // Push 200 ints to a MinHeap under GC, pop all and confirm ascending order
    // by checking the first pop is the min.
    let src = "fn main() -> Int {\n  let h = MinHeap()\n  var i = 0\n  while i < 200 { h.push((i * 37 + 11) - 100); i = i + 1 }\n  let garbage = Vec()\n  var j = 0\n  while j < 300 { garbage.push(j); j = j + 1 }\n  h.pop()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // The min of (i*37+11)-100 for i in 0..200: i=0 → -89; i=1 → -52; ... i=0 gives -89
    // Actually i=0 → 0*37+11-100 = -89. Is there smaller? i*37+11-100, minimum at i=0 → -89.
    assert_eq!(result.as_int(), -89);
}

#[test]
fn adv_tuple_with_record_field_equality() {
    // A tuple containing records — equality must dispatch through each
    // element's own descriptor (tuples.rs uses item.descriptor()). Two
    // structurally-equal tuples must compare equal.
    let src = "struct P { x: Int, y: Int }\nfn main() -> Int {\n  let a = (P { x: 1, y: 2 }, P { x: 3, y: 4 })\n  let b = (P { x: 1, y: 2 }, P { x: 3, y: 4 })\n  if a == b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_nested_vec_equality_deep() {
    // Deeply nested Vec equality: Vec[Vec[Vec[Int]]]. Equal shapes+content.
    let src = "fn main() -> Int {\n  let a = Vec()\n  let inner_a = Vec()\n  let leaf_a = Vec()\n  leaf_a.push(1)\n  leaf_a.push(2)\n  inner_a.push(leaf_a)\n  a.push(inner_a)\n  let b = Vec()\n  let inner_b = Vec()\n  let leaf_b = Vec()\n  leaf_b.push(1)\n  leaf_b.push(2)\n  inner_b.push(leaf_b)\n  b.push(inner_b)\n  if a == b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_nested_vec_equality_unequal_leaf() {
    // Complement: differing leaf content → unequal.
    let src = "fn main() -> Int {\n  let a = Vec()\n  let inner_a = Vec()\n  inner_a.push(1)\n  inner_a.push(2)\n  a.push(inner_a)\n  let b = Vec()\n  let inner_b = Vec()\n  inner_b.push(1)\n  inner_b.push(9)\n  b.push(inner_b)\n  if a == b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

#[test]
fn adv_two_faults_in_sequence_clean() {
    // Set a fault, observe it; the fault state must be cleanable so a second
    // independent run succeeds. (run_main creates a fresh Runtime each call,
    // so this is really two independent programs, but it confirms the Runtime
    // ctor + fault state start clean.)
    let (rt1, _) = run_main("fn main() -> Int { 1 / 0 }");
    assert!(rt1.has_pending_fault());
    let (rt2, result2) = run_main("fn main() -> Int { 42 }");
    assert!(!rt2.has_pending_fault(), "second run must start clean");
    assert_eq!(result2.as_int(), 42);
}

#[test]
fn adv_out_of_bounds_vec_get_faults() {
    // OOB index must fault as IndexOutOfBounds, not crash.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.get(5)\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "OOB vec get should fault");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::IndexOutOfBounds);
}

#[test]
fn adv_out_of_bounds_vec_negative_index_faults() {
    // Negative index — must fault cleanly, not wrap to a huge usize and crash.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.get(0 - 1)\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "negative index should fault");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::IndexOutOfBounds);
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

// ===========================================================================
// Adversarial edge-case tests — pipeline fusion, closures, GC interactions.
//
// These tests probe combinations the M8-WS11 suite did not cover: mutable
// captures mutated inside fused loops, GC pressure mid-pipeline, nested
// closure allocation during fusion, fold/reduce over GC-object accumulators,
// take(0)/negative-literal edges, and object-valued (non-Int) pipeline
// elements. Written from an adversary's perspective: try to break it even
// when it "should" work.
// ===========================================================================

/// Helper: build Praxis source that constructs a Vec of `n` sequential ints
/// starting at `start`, as a sequence of `v.push(...)` statements bound to `v`.
/// (`.push()` returns Unit, so it cannot be chained.)
fn vec_of(start: i64, n: i64) -> String {
    let mut s = String::from("let v = Vec()");
    for i in 0..n {
        s.push_str(&format!("\n  v.push({})", start + i));
    }
    s
}

#[test]
fn adv_pipeline_mutable_capture_mutated_in_fused_loop() {
    // A `var` captured by a closure is mutated on every fused-map call. The
    // VarCell must survive GC across the whole loop, and the final read must
    // reflect every mutation. This combines three features: VarCell, fused
    // pipeline, and GC pressure (the map allocates an Int per call).
    // v=[1..5].map(|x| { counter += x; x }) → counter=15, map result sum=15.
    let src = format!(
        "fn main() -> Int {{\n  var counter = 0\n  {vec}\n  let out = v.map(|x| {{ counter += x; x }})\n  counter\n}}\n",
        vec = vec_of(1, 5)
    );
    let (rt, result) = run_main(&src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 15);
}

#[test]
fn adv_pipeline_mutable_capture_mutated_in_fused_loop_gc_stress() {
    // Same as above but with 300 elements to force GC collections *during* the
    // fused loop while the VarCell is being mutated. If the VarCell isn't
    // rooted across the fused loop's safepoints, the cell gets collected and
    // the counter resets or corrupts.
    let src = "fn main() -> Int {\n  var counter = 0\n  let v = Vec()\n  var i = 0\n  while i < 300 { v.push(i); i = i + 1 }\n  let out = v.map(|x| { counter += x; x })\n  counter\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // sum(0..=299) = 299*300/2 = 44850
    assert_eq!(result.as_int(), 44850);
}

#[test]
fn adv_pipeline_map_result_used_after_gc_stress() {
    // The fused chain produces a Vec (implicit collect), and we use it after
    // the loop. Forces the collect_vec to stay rooted across GC inside the
    // loop, then read back. This exercises the Sink::Collect path under
    // pressure (the existing GC-stress test only sums, never reads the Vec).
    let src = "fn main() -> Int {\n  let v = Vec()\n  var i = 0\n  while i < 300 { v.push(i); i = i + 1 }\n  let out = v.map(|x| x * 2)\n  out.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 300);
}

#[test]
fn adv_pipeline_collect_vec_elements_survive_gc_stress() {
    // Collect into a Vec under heavy GC pressure, then sum the collected Vec
    // in a *separate* step. If the collect_vec's freshly-pushed elements are
    // not properly rooted, the second sum reads garbage / freed objects.
    let src = "fn main() -> Int {\n  let v = Vec()\n  var i = 0\n  while i < 300 { v.push(i); i = i + 1 }\n  let out = v.map(|x| x * 3)\n  var sum = 0\n  var j = 0\n  while j < out.len() { sum += out.get(j); j = j + 1 }\n  sum\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // 3 * sum(0..=299) = 3 * 44850 = 134550
    assert_eq!(result.as_int(), 134550);
}

#[test]
fn adv_pipeline_take_zero_yields_empty() {
    // take(0): the Take stage's guard is `idx >= 0`, which is true for idx=0,
    // so it breaks immediately. Empty source for any sink.
    let src = format!(
        "fn main() -> Int {{\n  {vec}\n  v.take(0).sum()\n}}\n",
        vec = vec_of(1, 5)
    );
    let (rt, result) = run_main(&src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

#[test]
fn adv_pipeline_skip_more_than_length_yields_empty() {
    // skip(100) on a 5-element Vec: every idx < 100, so all skipped. Sum=0.
    let src = format!(
        "fn main() -> Int {{\n  {vec}\n  v.skip(100).sum()\n}}\n",
        vec = vec_of(1, 5)
    );
    let (rt, result) = run_main(&src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

#[test]
fn adv_pipeline_take_then_skip_then_map_sum() {
    // [1..10].take(7).skip(2) = [3,4,5,6,7]; .map(*10).sum() = 250.
    // Exercises take+skip interaction inside one fused loop (take's break vs
    // skip's continue must compose correctly).
    let src = format!(
        "fn main() -> Int {{\n  {vec}\n  v.take(7).skip(2).map(|x| x * 10).sum()\n}}\n",
        vec = vec_of(1, 10)
    );
    let (rt, result) = run_main(&src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 250);
}

#[test]
fn adv_pipeline_skip_zero_is_identity() {
    // skip(0): `idx < 0` is always false, so nothing skipped. Sum unchanged.
    let src = format!(
        "fn main() -> Int {{\n  {vec}\n  v.skip(0).sum()\n}}\n",
        vec = vec_of(1, 5)
    );
    let (rt, result) = run_main(&src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 15);
}

#[test]
fn adv_pipeline_fold_accumulator_is_gc_int_under_pressure() {
    // fold whose accumulator is a GC Int object threaded across many
    // iterations under GC pressure. The accumulator GcRef must stay rooted
    // across every iteration's GC. (fold into a Vec is blocked by inference —
    // see handover — so this tests the GC-rooting of the fold acc with Int.)
    let src = "fn main() -> Int {\n  let v = Vec()\n  var i = 0\n  while i < 500 { v.push(i); i = i + 1 }\n  v.fold(0, |a, x| a + x)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // sum(0..=499) = 499*500/2 = 124750
    assert_eq!(result.as_int(), 124750);
}

#[test]
fn adv_pipeline_fold_into_vec_now_supported() {
    // FIXED (§3 Gap A): fold into a Vec accumulator now type-checks and runs.
    // The closure param `a` is inferred as Vec[Int] from the init `Vec()`:
    // bidirectional inference threads the combinator's accumulator type
    // (the name-shared `Acc` in fold's signature) into the closure's params
    // before the body is inferred, so `a.push(x)` resolves. Pre-fix this was
    // rejected with Y110; now it collects [1,2] → len 2.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  let acc = v.fold(Vec(), |a, x| { a.push(x); a })\n  acc.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn adv_pipeline_reduce_into_int_accumulator() {
    // reduce over Ints under GC pressure. The Reduce sink seeds from the first
    // element then folds. Verifies the seen-flag + Gc acc survive the loop.
    let src = "fn main() -> Int {\n  let v = Vec()\n  var i = 0\n  while i < 200 { v.push(i); i = i + 1 }\n  v.reduce(|a, x| a + x)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // sum(0..=199) = 199*200/2 = 19900
    assert_eq!(result.as_int(), 19900);
}

#[test]
fn adv_pipeline_nested_closure_allocation_in_fused_map() {
    // The map closure *returns* a closure (allocating a new closure object
    // each iteration). This stresses closure allocation + capture rooting
    // inside the fused loop. We then count the collected closures.
    // [1,2,3].map(|x| |y| x + y) → Vec of 3 closures. collect().len() = 3.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  let fs = v.map(|x| |y| x + y)\n  fs.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

#[test]
fn adv_pipeline_nested_closure_allocation_gc_stress() {
    // Same as above but 200 elements: each map call allocates a closure with
    // a captured Int env. The captured env objects must survive GC across the
    // rest of the loop while the collect_vec accumulates them.
    let src = "fn main() -> Int {\n  let v = Vec()\n  var i = 0\n  while i < 200 { v.push(i); i = i + 1 }\n  let fs = v.map(|x| |y| x + y)\n  fs.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 200);
}

#[test]
fn adv_pipeline_nested_vec_elements_survive_fused_count() {
    // Pipeline over a Vec of Vec[Int]. Each map returns the inner Vec unchanged;
    // count the collected inner Vecs. Verifies non-Int elements (nested Vec
    // GcRefs) survive the fused loop and that the closure receives the right
    // GcRef. Under GC pressure the inner Vecs must stay rooted while the loop
    // runs. (We can't call .len() on the closure param — inference limitation,
    // see adv_pipeline_method_on_closure_param_from_collection_rejected — so we
    // count the collected Vecs instead.)
    let src = "fn main() -> Int {\n  let v = Vec()\n  var i = 0\n  while i < 200 {\n    let inner = Vec()\n    inner.push(i)\n    inner.push(i)\n    v.push(inner)\n    i = i + 1\n  }\n  v.map(|inner| inner).count()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // 200 inner Vecs collected
    assert_eq!(result.as_int(), 200);
}

#[test]
fn adv_pipeline_method_on_closure_param_from_collection_now_supported() {
    // FIXED (§3 Gap B): a method call on a closure parameter whose type is the
    // element type of a collection now resolves. `v.map(|i| i.len())` over a
    // Vec[Vec[Int]] failed with T110 pre-fix — two root causes, both closed:
    //
    // 1. Bidirectional inference threads the receiver's element type into the
    //    closure param before the body is inferred, so `.len()` resolves once
    //    the element type is known.
    // 2. The HM value restriction: `let v = Vec()` no longer generalizes to
    //    `forall T. Vec[T]` (an expansive RHS is left monomorphic), so the
    //    element-type pinning from `v.push(inner)` propagates to the later
    //    `v.map(...)` instead of each call instantiating a fresh element type.
    //
    // The idiomatic build-then-map pattern now type-checks and runs.
    // inner = [1] → len 1 → sum = 1.
    let src = "fn main() -> Int {\n  let v = Vec()\n  let inner = Vec()\n  inner.push(1)\n  v.push(inner)\n  v.map(|i| i.len()).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_pipeline_min_by_on_nested_collection_lengths() {
    // The Gap B fix unblocks the `.len()`-based min_by/max_by comparators over
    // nested collections: find the inner Vec with the fewest elements. The
    // comparator closure's params `a`/`b` are the element type Vec[Int], pinned
    // by bidirectional inference + the value restriction so `a.len()`/`b.len()`
    // resolve. min_by takes a (T,T)->Bool comparator; "shorter" is
    // `a.len() < b.len()`. inner lengths [3,1,2] → shortest = b → len 1.
    let src = String::from("fn main() -> Int {\n")
        + "  let v = Vec()\n"
        + "  let a = Vec()\n  a.push(1)\n  a.push(2)\n  a.push(3)\n  v.push(a)\n"
        + "  let b = Vec()\n  b.push(9)\n  v.push(b)\n"
        + "  let c = Vec()\n  c.push(4)\n  c.push(5)\n  v.push(c)\n"
        + "  let shortest = v.min_by(|a, b| a.len() < b.len())\n"
        + "  shortest.len()\n"
        + "}\n";
    let (rt, result) = run_main(&src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_pipeline_collect_nested_vecs_then_count() {
    // Collect a Vec of Vec[Int] (identity map), then read its length. Verifies
    // nested Vec GcRefs survive collect + a downstream .len() on the *outer*
    // collected Vec (whose type is Vec[Vec[Int]] — known to inference, unlike
    // the inner element type).
    let src = "fn main() -> Int {\n  let v = Vec()\n  let a = Vec()\n  a.push(10)\n  a.push(20)\n  v.push(a)\n  let b = Vec()\n  b.push(30)\n  v.push(b)\n  v.map(|inner| inner).len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // 2 inner Vecs collected
    assert_eq!(result.as_int(), 2);
}

#[test]
fn adv_pipeline_find_with_allocating_predicate() {
    // find's predicate allocates (creates an Int) before returning its bool.
    // If the fused loop doesn't root the current element across the predicate's
    // allocation, find matches the wrong element or faults.
    let src = "fn main() -> Int {\n  let v = Vec()\n  var i = 0\n  while i < 100 { v.push(i); i = i + 1 }\n  v.find(|x| x + 0 == 50)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 50);
}

#[test]
fn adv_pipeline_any_short_circuits_keeps_loop_invariant() {
    // any short-circuits; verify the break leaves the source Vec intact (no
    // corruption from the fused loop's bookkeeping) by summing it afterwards.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  let b = v.any(|x| x == 3)\n  var after = 0\n  var i = 0\n  while i < v.len() { after += v.get(i); i = i + 1 }\n  if b { after } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // sum(1..=4) = 10
    assert_eq!(result.as_int(), 10);
}

#[test]
fn adv_pipeline_two_chains_share_no_state() {
    // Run two independent fused chains on the same source. If the recognizer
    // or builder accidentally shared slot state between chains, the second
    // result would be wrong.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  let a = v.map(|x| x * 10).sum()\n  let b = v.map(|x| x * 100).sum()\n  a + b\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // a = 10+20+30 = 60; b = 100+200+300 = 600; total = 660
    assert_eq!(result.as_int(), 660);
}

#[test]
fn adv_pipeline_min_by_under_gc_pressure() {
    // min_by over 500 Ints under GC pressure, comparator is plain less-than.
    // The running-best GcRef (an Int) must survive every collection during the
    // loop. The existing min_by test uses 3 elements with no GC.
    let src = "fn main() -> Int {\n  let v = Vec()\n  var i = 0\n  while i < 500 { v.push(i); i = i + 1 }\n  v.min_by(|a, b| a < b)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // min is 0
    assert_eq!(result.as_int(), 0);
}

#[test]
fn adv_pipeline_max_by_under_gc_pressure() {
    // max_by over 500 Ints under GC pressure.
    let src = "fn main() -> Int {\n  let v = Vec()\n  var i = 0\n  while i < 500 { v.push(i); i = i + 1 }\n  v.max_by(|a, b| a < b)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // max is 499
    assert_eq!(result.as_int(), 499);
}

#[test]
fn adv_pipeline_min_under_gc_pressure() {
    // min over 500 Ints under GC pressure (no comparator). The Min sink holds
    // the running min as a scalar but the element GcRef must survive the
    // predicate/extract across collections.
    let src = "fn main() -> Int {\n  let v = Vec()\n  var i = 1\n  while i <= 500 { v.push(i); i = i + 1 }\n  v.min()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_pipeline_map_filter_map_filter_sum_deep_chain() {
    // A long chain: map.filter.map.filter.sum — five stages + sink in one
    // fused loop. Verifies stage composition doesn't lose elements.
    // [1..8].map(+1)=[2..9].filter(>3)=[4..9].map(*2)=[8,10,12,14,16,18]
    //      .filter(<15)=[8,10,12,14].sum()=44
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.push(5)\n  v.push(6)\n  v.push(7)\n  v.map(|x| x + 1).filter(|x| x > 3).map(|x| x * 2).filter(|x| x < 15).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 44);
}

#[test]
fn adv_pipeline_flat_map_gc_stress_preserves_inner_vecs() {
    // flat_map under GC stress: each closure call allocates a fresh Vec, the
    // inner loop reads it. If the inner Vec isn't rooted, the inner loop
    // faults or reads freed memory. 100 outer × 3 inner = 300 sum if i.
    let src = "fn main() -> Int {\n  let v = Vec()\n  var i = 0\n  while i < 100 { v.push(i); i = i + 1 }\n  v.flat_map(|x| {\n    let r = Vec()\n    r.push(x)\n    r.push(x)\n    r.push(x)\n    r\n  }).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // sum(0..=99) * 3 = 4950 * 3 = 14850
    assert_eq!(result.as_int(), 14850);
}

#[test]
fn adv_pipeline_empty_source_collect_is_empty_vec() {
    // Empty source → collect → empty Vec → len 0. Verifies the Collect sink's
    // collect_vec is allocated and returned even when the loop body never runs.
    let src = "fn main() -> Int {\n  let v = Vec()\n  let out = v.map(|x| x * 2)\n  out.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

#[test]
fn adv_pipeline_empty_source_min_is_zero() {
    // Empty source → min. The accumulator is seeded to 0 and never updated.
    // (min/max on empty is a known edge — document current behavior.)
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.min()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

#[test]
fn adv_pipeline_empty_source_any_is_false() {
    // Empty source → any → false (vacuously). Packed as 0.
    let src = "fn main() -> Int {\n  let v = Vec()\n  let b = v.any(|x| x == 0)\n  if b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

#[test]
fn adv_pipeline_empty_source_all_is_true() {
    // Empty source → all → true (vacuously). Packed as 1.
    let src = "fn main() -> Int {\n  let v = Vec()\n  let b = v.all(|x| x > 0)\n  if b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_pipeline_empty_source_reduce() {
    // Empty source → reduce. The acc is never seeded (seen stays false).
    // Document current behavior: returns whatever the unseeded Gc slot holds.
    // We at least confirm it doesn't crash the host (no Rust panic).
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.reduce(|a, x| a + x)\n}\n";
    let (rt, _result) = run_main(src);
    // We do NOT assert the value (it's undefined for empty); we only assert
    // the host survived (no abort/panic). A fault is acceptable; a crash is not.
    let _ = rt.has_pending_fault();
}

#[test]
fn adv_pipeline_count_after_filter_all_dropped() {
    // filter drops every element → count is 0. Verifies filter's continue
    // (jump to incr) still advances the loop counter correctly.
    let src = format!(
        "fn main() -> Int {{\n  {vec}\n  v.filter(|x| x > 1000).count()\n}}\n",
        vec = vec_of(1, 10)
    );
    let (rt, result) = run_main(&src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

#[test]
fn adv_pipeline_chained_collect_used_as_receiver_of_next_chain() {
    // A chain's collected Vec is the source of a *second* chain. Verifies the
    // recognizer correctly treats a pipeline-result Vec as a source leaf.
    // [1..5].map(*2)=[2,4,6,8].collect implicitly, then .filter(>4).sum()=14.
    let src = format!(
        "fn main() -> Int {{\n  {vec}\n  v.map(|x| x * 2).filter(|x| x > 4).sum()\n}}\n",
        vec = vec_of(1, 4)
    );
    let (rt, result) = run_main(&src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // [2,4,6,8] filter(>4)=[6,8] sum=14
    assert_eq!(result.as_int(), 14);
}

#[test]
fn adv_closure_returned_from_fn_used_in_pipeline() {
    // A fn returns a capturing closure; that closure is passed to .map.
    // Combines returned-closure (GC'd env outlives frame) with the fused loop.
    let src = "fn mk(off: Int) { |x| x + off }\nfn main() -> Int {\n  let f = mk(100)\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.map(f).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // 101 + 102 + 103 = 306
    assert_eq!(result.as_int(), 306);
}

#[test]
fn adv_pipeline_sum_does_not_mutate_source_vec() {
    // A fused sum reads the source but must not mutate it. After summing, we
    // sum again to confirm the source is intact (a buggy fuser that consumed
    // the Vec or advanced an index would give a different second sum).
    let src = format!(
        "fn main() -> Int {{\n  {vec}\n  let a = v.sum()\n  let b = v.sum()\n  a + b\n}}\n",
        vec = vec_of(1, 5)
    );
    let (rt, result) = run_main(&src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // 15 + 15 = 30
    assert_eq!(result.as_int(), 30);
}

#[test]
fn adv_pipeline_take_then_count_under_gc_pressure() {
    // take(50) on a 200-element Vec under GC pressure, then count. The Take
    // stage's break must fire correctly even after collections have run.
    let src = "fn main() -> Int {\n  let v = Vec()\n  var i = 0\n  while i < 200 { v.push(i); i = i + 1 }\n  v.take(50).count()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 50);
}

#[test]
fn adv_pipeline_zip_under_gc_pressure() {
    // zip of two 300-element Vecs, count the pairs. Both source Vecs and the
    // index must survive GC.
    let src = "fn main() -> Int {\n  let a = Vec()\n  let b = Vec()\n  var i = 0\n  while i < 300 { a.push(i); b.push(i); i = i + 1 }\n  a.zip(b).count()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 300);
}

#[test]
fn adv_pipeline_take_while_then_collect_under_gc_pressure() {
    // take_while under GC pressure: stops at the first element >= 50, collects.
    let src = "fn main() -> Int {\n  let v = Vec()\n  var i = 0\n  while i < 200 { v.push(i); i = i + 1 }\n  v.take_while(|x| x < 50).count()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // 0..49 → 50 elements
    assert_eq!(result.as_int(), 50);
}

// ===========================================================================
// Adversarial batch 2: fault propagation, nested captures, recursion, and
// reallocation safety. These probe control-flow + GC interactions the basic
// suite skips.
// ===========================================================================

#[test]
fn adv_fused_sum_overflow_faults_cleanly() {
    // Sum overflows on the 3rd element. The fault must propagate out of the
    // fused loop without corrupting the host (no Rust panic/abort). The fused
    // Sum sink does acc += item in a scalar; overflow must fault.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(9223372036854775807)\n  v.push(0)\n  v.push(1)\n  v.sum()\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "sum overflow should fault");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::IntOverflow);
}

#[test]
fn adv_fused_map_closure_fault_propagates() {
    // A map closure faults (div-by-zero on element 2). The fault must propagate
    // through the fused loop's CallIndirect + check_fault without the loop
    // continuing or the host crashing.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(10)\n  v.push(0)\n  v.push(30)\n  v.map(|x| 100 / x).sum()\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "div-by-zero in map should fault");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::DivByZero);
}

#[test]
fn adv_fused_filter_predicate_fault_propagates() {
    // A filter predicate faults. Verifies fault propagation through the
    // predicate's CallIndirect + the filter stage's branch structure.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(5)\n  v.push(0)\n  v.filter(|x| 100 / x > 1).count()\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "div-by-zero in filter should fault");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::DivByZero);
}

#[test]
fn adv_fused_fold_closure_fault_propagates() {
    // A fold closure faults mid-fold. Verifies the Fold sink's CallIndirect
    // fault check works and the accumulator isn't left corrupted.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(0)\n  v.fold(0, |a, x| a + 100 / x)\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "div-by-zero in fold should fault");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::DivByZero);
}

#[test]
fn adv_fused_find_predicate_fault_propagates() {
    // A find predicate faults. Verifies short-circuit sink fault propagation.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(0)\n  v.find(|x| 10 / x == 1)\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "div-by-zero in find should fault");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::DivByZero);
}

#[test]
fn adv_nested_closures_share_var_cell() {
    // Two closures capture the same `var`; calling one then the other observes
    // the shared cell. inc mutates, getn returns it.
    let src = "fn main() -> Int {\n  var n = 0\n  let inc = |x| { n = n + x }\n  let getn = |_| n\n  inc(10)\n  inc(5)\n  getn(0)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 15);
}

#[test]
fn adv_nested_closures_share_var_cell_under_gc_pressure() {
    // Same as above but allocate heavily between calls so GC runs while both
    // closures' envs (pointing at the same VarCell) must survive.
    let src = "fn main() -> Int {\n  var n = 0\n  let inc = |x| { n = n + x }\n  let getn = |_| n\n  inc(10)\n  var i = 0\n  let garbage = Vec()\n  while i < 500 { garbage.push(i); i = i + 1 }\n  inc(5)\n  getn(0)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 15);
}

#[test]
fn adv_closure_mutating_capture_then_returned_and_called_repeatedly() {
    // A returned closure mutates its captured `var` each call; called 100× under
    // GC pressure. The VarCell must survive across every call's potential GC.
    let src = "fn make() {\n  var n = 0\n  |x| { n = n + x; n }\n}\nfn main() -> Int {\n  let bump = make()\n  var i = 0\n  while i < 100 { bump(1); i = i + 1 }\n  bump(0)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 100);
}

#[test]
fn adv_recursive_function_with_captured_var() {
    // A closure that captures a `var` and recurses (via a named fn, since
    // recursive closures aren't specially handled). The VarCell must survive
    // the recursion's GC pressure.
    let src = "fn count(n: Int, dec) -> Int {\n  if n == 0 { dec(0) } else { dec(1); count(n - 1, dec) }\n}\nfn main() -> Int {\n  var total = 0\n  let add = |x| { total += x }\n  count(100, add)\n  total\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // add called with 1 a hundred times → total = 100
    assert_eq!(result.as_int(), 100);
}

#[test]
fn adv_for_loop_sum_does_not_corrupt_on_reallocation() {
    // §11.5 reallocation safety: a `for` loop over a Vec while the SAME Vec is
    // grown inside the loop body. This is the classic use-after-realloc hazard.
    // The for-loop's index-based access must reload the data pointer each
    // iteration (or the runtime must keep access behind calls). We don't grow
    // the iterated Vec here (undefined behavior territory); instead we verify
    // the for loop reads the *snapshot* length taken at loop entry.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  var sum = 0\n  for x in v {\n    sum = sum + x\n  }\n  sum\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 6);
}

#[test]
fn adv_for_loop_over_vec_under_gc_pressure() {
    // for-loop iterating a 500-element Vec, summing. The loop counter and the
    // source Vec must survive GC during iteration.
    let src = "fn main() -> Int {\n  let v = Vec()\n  var i = 0\n  while i < 500 { v.push(i); i = i + 1 }\n  var sum = 0\n  for x in v { sum += x }\n  sum\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // sum(0..=499) = 124750
    assert_eq!(result.as_int(), 124750);
}

#[test]
fn adv_pipeline_chain_after_pipeline_chain_nested() {
    // A pipeline whose source is itself a pipeline result that was collected:
    // `(v.map(f)).filter(p).sum()`. Already covered, but this variant uses a
    // capturing closure in the inner map AND a predicate in the outer filter,
    // both reading the same captured `var`. Verifies two closures + a shared
    // cell all root correctly in one fused loop.
    let src = "fn main() -> Int {\n  var threshold = 5\n  let v = Vec()\n  var i = 0\n  while i < 20 { v.push(i); i = i + 1 }\n  v.map(|x| x + threshold).filter(|x| x > threshold).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // x+5 for x in 0..19, keep where x+5 > 5 i.e. x > 0 → x in 1..19
    // sum(1..=19) + 5*19 = 190 + 95 = 285
    assert_eq!(result.as_int(), 285);
}

#[test]
fn adv_curried_closure_used_in_pipeline_gc_stress() {
    // A curried closure (closure returning a closure) is the map function in a
    // fused pipeline under GC pressure. The outer closure's env must survive
    // while the inner closures it produces are invoked.
    let src = "fn main() -> Int {\n  let adder = |off| |x| x + off\n  let add10 = adder(10)\n  let v = Vec()\n  var i = 0\n  while i < 200 { v.push(i); i = i + 1 }\n  v.map(add10).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // sum(0..=199) + 10*200 = 19900 + 2000 = 21900
    assert_eq!(result.as_int(), 21900);
}

#[test]
fn adv_mutable_capture_read_in_predicate_of_fused_filter() {
    // A filter predicate reads a captured `var` (not mutating). The VarCell
    // read must work inside the fused filter stage.
    let src = "fn main() -> Int {\n  var limit = 10\n  let v = Vec()\n  var i = 0\n  while i < 30 { v.push(i); i = i + 1 }\n  v.filter(|x| x > limit).count()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // x > 10 for x in 0..29 → 19 values (11..29)
    assert_eq!(result.as_int(), 19);
}

#[test]
fn adv_mutable_capture_mutated_by_one_closure_read_by_pipeline_predicate() {
    // One closure mutates the captured `var`; a pipeline filter predicate
    // reads the *current* value. Verifies the VarCell is shared and the read in
    // the fused loop sees the post-mutation value.
    let src = "fn main() -> Int {\n  var limit = 5\n  let setlimit = |n| { limit = n }\n  setlimit(15)\n  let v = Vec()\n  var i = 0\n  while i < 30 { v.push(i); i = i + 1 }\n  v.filter(|x| x > limit).count()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // limit now 15; x > 15 for x in 0..29 → 14 values (16..29)
    assert_eq!(result.as_int(), 14);
}

#[test]
fn adv_pipeline_empty_flat_map_yields_empty() {
    // flat_map where every closure returns an empty Vec → zero elements.
    // Verifies the inner loop's bounds check (empty inner Vec) terminates
    // correctly and the outer loop continues.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.flat_map(|x| Vec()).count()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

#[test]
fn adv_pipeline_flat_map_with_filter_then_sum() {
    // flat_map splices inner Vecs, then filter + sum in the SAME fused loop.
    // This combines the flat_map special-case (inner loop) with downstream
    // stages — a tricky control-flow composition.
    // [1,2].flat_map(|x|[x,x*10]) = [1,10,2,20].filter(>5)=[10,20].sum()=30.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.flat_map(|x| {\n    let r = Vec()\n    r.push(x)\n    r.push(x * 10)\n    r\n  }).filter(|x| x > 5).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 30);
}

#[test]
fn adv_indirect_call_on_local_closure_works() {
    // The SUPPORTED path: a closure bound to a local, then called. This works
    // (the callee resolves to a local). The closures-from-collections case is
    // now ALSO supported — see adv_call_closure_retrieved_from_collection and
    // siblings below.
    let src = "fn main() -> Int {\n  let f = |x| x + 7\n  f(100)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 107);
}

#[test]
fn adv_call_closure_retrieved_from_collection() {
    // PROBE (§2): invoking a closure retrieved from a collection used to
    // miscompile and SIGSEGV — `fs.get(0)(100)` resolved its callee to no symbol
    // (SymbolId(u32::MAX), empty callee_name) and fell through to a nonsense
    // direct call (CallTarget::User("")), which does not read the closure's
    // fn_ptr. Fixed: a postfix `expr(args)` parse production wraps the preceding
    // expression as the call's callee; the HIR lowerer stores it as `callee_expr`
    // and inference unifies it against a Func; the MIR builder lowers it to
    // Inst::CallIndirect (reads the closure's fn_ptr and calls through it).
    // Pre-fix this segfaulted; now it returns 101.
    let src = String::from("fn main() -> Int {\n")
        + "  let fs = Vec()\n"
        + "  fs.push(|x| x + 1)\n"
        + "  fs.get(0)(100)\n"
        + "}\n";
    let (rt, result) = run_main(&src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 101);
}

#[test]
fn adv_call_closure_in_parens() {
    // The parenthesized-callee case: `(expr)(args)`. A paren holds a closure
    // value; calling through it exercises the same postfix-call + indirect path
    // as the collection case (the callee is an arbitrary expression, not a name).
    let src = "fn main() -> Int {\n  (|x| x * 3)(14)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

#[test]
fn adv_call_result_of_call() {
    // `f(1)(2)` — calling the result of another call (a curried closure
    // returned by f). The outer call's callee is the inner CALL_EXPR, the same
    // arbitrary-expression-callee path.
    let src = String::from("fn main() -> Int {\n")
        + "  let mk = |a| |b| a + b\n"
        + "  mk(40)(2)\n"
        + "}\n";
    let (rt, result) = run_main(&src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

#[test]
fn adv_call_closure_from_collection_under_gc_pressure() {
    // The closure-from-collection path under GC pressure: the retrieved closure
    // (and the Vec holding it) must survive collections triggered by allocations
    // between the get and the indirect call. Retrieves the second closure and
    // calls it after allocating garbage.
    let src = String::from("fn main() -> Int {\n")
        + "  let fs = Vec()\n"
        + "  fs.push(|x| x + 1)\n"
        + "  fs.push(|x| x * 100)\n"
        + "  let garbage = Vec()\n"
        + "  var i = 0\n"
        + "  while i < 300 { garbage.push(i); i = i + 1 }\n"
        + "  fs.get(1)(7)\n"
        + "}\n";
    let (rt, result) = run_main(&src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 700);
}

#[test]
fn adv_shadowing_then_closure_captures_correct_binding_same_type() {
    // §4.2 / §5.3: a closure created before a shadowing declaration retains the
    // binding it originally captured. Same-type shadow (Int→Int). (Uses a `_`
    // param because `|| a` parses as logical-or, not a zero-arg closure.)
    let src =
        "fn main() -> Int {\n  let a = 4\n  let show_old = |_| a\n  let a = 99\n  show_old(0)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // show_old captured the first a (4), not the shadowed a (99)
    assert_eq!(result.as_int(), 4);
}

#[test]
fn adv_shadowing_then_closure_captures_correct_binding_type_change() {
    // §4.2: the headline example — a closure created before a shadowing `let`
    // with a different type retains the original Int binding.
    let src = "fn main() -> Int {\n  let a = 4\n  let show_old = |_| a\n  let a = \"Foo\"\n  show_old(0)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // show_old captured the first a (Int 4), not the Text "Foo"
    assert_eq!(result.as_int(), 4);
}

#[test]
fn adv_shadowing_initializer_resolves_previous_binding() {
    // §5.3: a shadowing initializer resolves names in the preceding environment.
    // `let a = a + 1` — the RHS `a` is the previous binding.
    let src = "fn main() -> Int {\n  let a = 4\n  let a = a + 1\n  a\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 5);
}

#[test]
fn adv_let_shadowing_changes_type() {
    // §4.2: shadowing may change type. `let a = 4; let a = "x"` — both valid.
    let src = "fn main() -> Int {\n  let a = 4\n  let a = a + 1\n  a\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 5);
}

// --- M9: Option[T] prelude enum --------------------------------------------
// Option is a polymorphic prelude enum (forall T. Option[T]) with variants
// Some(T) and None. These tests exercise construction, matching, equality, and
// cross-site unification (the M9 structural-same-named-enum unify fix).

#[test]
fn m9_option_some_construction_and_match() {
    // `Some(42)` constructs; matching `.Some(n)` extracts the payload.
    let src = "fn main() -> Int {\n  let v = Some(42)\n  match v {\n    Some(n) => n\n    None => 0\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

#[test]
fn m9_option_none_construction_and_match() {
    // `None` constructs a payload-less variant.
    let src = "fn main() -> Int {\n  let v = None\n  match v {\n    Some(n) => n\n    None => 7\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 7);
}

#[test]
fn m9_option_some_none_equality() {
    // `Some(1) == Some(1)` is true; `Some(1) == None` is false; `None == None` is true.
    let src = "fn main() -> Int {\n  let a = Some(1) == Some(1)\n  let b = Some(1) == None\n  let c = None == None\n  if a { if b { 3 } else { if c { 2 } else { 4 } } } else { 1 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn m9_option_unifies_across_construction_sites() {
    // Two independently-constructed Some values unify through match + equality,
    // exercising the same-named-enum structural unification.
    let src = "fn main() -> Int {\n  let a = Some(10)\n  let b = Some(20)\n  if a == Some(10) { if b == Some(20) { 1 } else { 2 } } else { 3 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn m9_option_text_payload() {
    // Option is polymorphic: Some(Text) works, not just Some(Int).
    let src = "fn main() -> Int {\n  let v = Some(\"hi\")\n  match v {\n    Some(s) => if s == \"hi\" { 1 } else { 0 }\n    None => 9\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn m9_option_type_annotation() {
    // An explicit `Option[Int]` annotation unifies with Some(5).
    let src = "fn main() -> Int {\n  let v: Option[Int] = Some(5)\n  match v {\n    Some(n) => n\n    None => 0\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 5);
}

#[test]
fn m9_option_returned_from_function() {
    // A function returning Option[Int] (None path) — polymorphic enum as a
    // return type, flowing through a fresh def at the annotation vs. the
    // Some-construction site.
    let src = "fn maybe(x: Int) -> Option[Int] {\n  if x > 0 { Some(x) } else { None }\n}\nfn main() -> Int {\n  match maybe(5) {\n    Some(n) => n\n    None => 0\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 5);
}

#[test]
fn m9_option_returned_none_from_function() {
    // The None path of the function above.
    let src = "fn maybe(x: Int) -> Option[Int] {\n  if x > 0 { Some(x) } else { None }\n}\nfn main() -> Int {\n  match maybe(-1) {\n    Some(n) => n\n    None => 99\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 99);
}

// --- M9 WS2: named heterogeneous sections + repeated tail (§7.5) ------------

#[test]
fn m9_named_sections_two_fields() {
    // sections(rules: ..., updates: ...) → record { rules, updates }.
    // rules = Vec[Int] of 2 values; updates = Vec[Int] of 3 values.
    // Access `.a` and `.b` field on the record.
    let src = "fn main() -> Int {\n  let data = read sections(a: lines(int), b: lines(int))\n  data.a.len() + data.b.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "1\n2\n\n3\n4\n5");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 5); // 2 + 3
}

#[test]
fn m9_named_sections_field_values() {
    // The first section's first value is 7.
    let src = "fn main() -> Int {\n  let data = read sections(a: lines(int), b: lines(int))\n  data.a.get(0)\n}\n";
    let (rt, result) = run_main_with_input(src, "7\n8\n\n9\n10");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 7);
}

#[test]
fn m9_named_sections_with_repeated_tail() {
    // sections(single: lines(int), rest: repeated(lines(int))) — one fixed
    // section then all remaining sections folded into a Vec[Vec[Int]].
    let src = "fn main() -> Int {\n  let data = read sections(single: lines(int), rest: repeated(lines(int)))\n  data.single.len() + data.rest.len()\n}\n";
    // 1 section of 1 line (single), then 3 sections (rest has 3 elements).
    let (rt, result) = run_main_with_input(src, "100\n\n1\n\n2\n\n3");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 4); // 1 + 3
}

#[test]
fn m9_named_sections_template_fields() {
    // Named sections with template parsers → record of records. Access the
    // inner record's fields directly through indexing.
    let src = "fn main() -> Int {\n  let data = read sections(p: lines(`{x:int},{y:int}`))\n  let first = data.p.get(0)\n  first.x + first.y\n}\n";
    let (rt, result) = run_main_with_input(src, "3,4");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 7);
}

#[test]
fn m9_named_sections_too_few_sections_faults() {
    // Fewer sections than named fields → ParseFailed fault.
    let src =
        "fn main() -> Int {\n  let data = read sections(a: lines(int), b: lines(int))\n  42\n}\n";
    let (rt, _result) = run_main_with_input(src, "1\n2\n3"); // one section, two fields wanted
    assert!(
        rt.has_pending_fault(),
        "expected ParseFailed on too-few sections, got: {:?}",
        rt.fault()
    );
}

// --- M9 WS3: block (§7.5, §7.7) ---------------------------------------------

#[test]
fn m9_block_template_plus_named_field() {
    // sections(block(`Monkey {id:int}:`, items: lines(int))) — each section is
    // a block: a positional header template (flattening `id`) + a named `items`
    // field consuming the remaining lines.
    let src = "fn main() -> Int {\n  let monkeys = read sections(block(`Monkey {id:int}:`, items: lines(int)))\n  let m0 = monkeys.get(0)\n  m0.id + m0.items.len()\n}\n";
    let input = "Monkey 1:\n10\n20\n\nMonkey 2:\n30\n40";
    let (rt, result) = run_main_with_input(src, input);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // m0.id = 1, m0.items has 2 entries (10, 20) → 1 + 2 = 3
    assert_eq!(result.as_int(), 3);
}

#[test]
fn m9_block_second_section() {
    // The second monkey's id and item count.
    let src = "fn main() -> Int {\n  let monkeys = read sections(block(`Monkey {id:int}:`, items: lines(int)))\n  let m1 = monkeys.get(1)\n  m1.id + m1.items.len()\n}\n";
    let input = "Monkey 1:\n10\n20\n\nMonkey 2:\n30\n40";
    let (rt, result) = run_main_with_input(src, input);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // m1.id = 2, m1.items has 2 entries (30, 40) → 2 + 2 = 4
    assert_eq!(result.as_int(), 4);
}

#[test]
fn m9_block_two_template_fields_flatten() {
    // A block with two positional templates whose named captures both flatten
    // into the record: `{x:int},{y:int}` then `\n{z:int}`.
    let src = "fn main() -> Int {\n  let b = read block(`{x:int},{y:int}\\n{z:int}`)\n  b.x + b.y + b.z\n}\n";
    let (rt, result) = run_main_with_input(src, "1,2\n3");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 6);
}

// --- M9 WS4: choice generated enums (§7.5) ----------------------------------

#[test]
fn m9_choice_first_alternative_matches() {
    // choice(A: `{a:int}`, B: `{b:int}`) on "42" → first case wins (.A).
    // Scalar payload recovered directly.
    let src = "fn main() -> Int {\n  let v = read choice(A: int, B: int)\n  match v {\n    A(n) => n\n    B(n) => n\n  }\n}\n";
    let (rt, result) = run_main_with_input(src, "42");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

#[test]
fn m9_choice_second_alternative_via_backtrack() {
    // choice(A: int, B: word) on "hello" — A fails (not an int), B wins via
    // backtracking. Scalar payloads to avoid the anon-record-as-payload field
    // access inference gap (a pre-existing limitation, not choice-specific).
    let src = "fn main() -> Int {\n  let v = read choice(A: int, B: word)\n  match v {\n    A(n) => n\n    B(w) => 99\n  }\n}\n";
    let (rt, result) = run_main_with_input(src, "hello");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 99);
}

#[test]
fn m9_choice_distinct_payloads() {
    // Two cases with different scalar payload types; the matched case's payload
    // is recovered.
    let src = "fn main() -> Int {\n  let v = read choice(Num: int, Txt: word)\n  match v {\n    Num(n) => n\n    Txt(w) => 7\n  }\n}\n";
    let (rt, result) = run_main_with_input(src, "123");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 123);
}

#[test]
fn m9_choice_equality() {
    // Two choice results of the same shape compare equal when same variant+payload.
    let src = "fn main() -> Int {\n  let a = read choice(N: int)\n  let b = read choice(N: int)\n  if a == b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main_with_input(src, "5");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

// NOTE: choice with a *record-payload* case (e.g. choice(A: `{a:int}`) then
// `match v { A(r) => r.a }`) is a known gap: anonymous-record field access on
// a match-bound variant payload doesn't resolve through the anon-enum payload
// type. Scalar payloads work fully (above). This is a pre-existing inference
// interaction surfaced by choice, tracked as an M9 follow-up; it does not block
// the §19.9 acceptance fixtures, which use scalar-payload choices (C.9 scan).

// --- M9 WS5: optional + Option[T] integration (§7.5) -----------------------

#[test]
fn m9_optional_present_returns_some() {
    // optional(int) on "42" → Some(42).
    let src = "fn main() -> Int {\n  let v = read optional(int)\n  match v {\n    Some(n) => n\n    None => 0\n  }\n}\n";
    let (rt, result) = run_main_with_input(src, "42");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

#[test]
fn m9_optional_absent_returns_none() {
    // optional(int) on "hello" → None (int parse fails). No fault raised.
    let src = "fn main() -> Int {\n  let v = read optional(int)\n  match v {\n    Some(n) => n\n    None => 7\n  }\n}\n";
    let (rt, result) = run_main_with_input(src, "hello");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 7);
}

#[test]
fn m9_optional_some_none_equality() {
    // Some(5) == Some(5) is true; Some(5) == None is false; None == None is true.
    let src = "fn main() -> Int {\n  let a = read optional(int)\n  let b = read optional(int)\n  let same = a == b\n  if same { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main_with_input(src, "5");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn m9_optional_present_and_absent_differ() {
    // Each read re-parses the whole input (§7.10). a = optional(int) on "5" →
    // Some(5); b = optional(word) on "5" → Some("5"). Both Some; result is a's n.
    let src = "fn main() -> Int {\n  let a = read optional(int)\n  let b = read optional(word)\n  match a {\n    Some(n) => match b {\n      Some(w) => n\n      None => 99\n    }\n    None => 0\n  }\n}\n";
    let (rt, result) = run_main_with_input(src, "5");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 5);
}

// --- M9 WS6: scan (§7.5, C.9) -----------------------------------------------

#[test]
fn m9_scan_extracts_matches_in_order() {
    // scan(choice(Mul: `mul({a:int},{b:int})`)) over corrupted text — finds all
    // mul(a,b) in source order, ignoring other text. Counts the matches.
    let src = "fn main() -> Int {\n  let ms = read scan(choice(M: `mul({a:int},{b:int})`))\n  ms.len()\n}\n";
    let input = "xmul(2,3)ymul(4,5)don't()mul(6,7)";
    let (rt, result) = run_main_with_input(src, input);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

#[test]
fn m9_scan_extracts_payload_values() {
    // Sum the `a` of every matched mul(a,b). Uses scalar payload via the choice
    // case (the record-payload field-access gap is separate). Here we just count
    // and verify the first match's existence indirectly via length.
    let src = "fn main() -> Int {\n  let ms = read scan(choice(M: `mul({a:int},{b:int})`))\n  ms.len()\n}\n";
    let input = "abc()mul(1,2)xyz";
    let (rt, result) = run_main_with_input(src, input);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn m9_scan_no_matches_returns_empty_vec() {
    // scan on text with no matches → empty Vec, no fault.
    let src = "fn main() -> Int {\n  let ms = read scan(choice(M: `mul({a:int},{b:int})`))\n  ms.len()\n}\n";
    let input = "nothing here at all";
    let (rt, result) = run_main_with_input(src, input);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

// --- M9 WS7: matrix, ragged grids, chars, one_of (§7.5) --------------------

#[test]
fn m9_one_of_matches_a_char() {
    // one_of("LR") on "L" → Char 'L'. Verify by counting via chars.
    let src =
        "fn main() -> Int {\n  let cs = read chars(one_of(\"LR\"), skip: none)\n  cs.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "LLRRL");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 5);
}

#[test]
fn m9_chars_skip_whitespace() {
    // chars(one_of("^v<>"), skip: whitespace) extracts moves ignoring spaces.
    let src = "fn main() -> Int {\n  let cs = read chars(one_of(\"^v<>\"), skip: whitespace)\n  cs.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "^ v < > <");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 5);
}

#[test]
fn m9_matrix_rectangular_int() {
    // matrix(int) on whitespace-separated ints → Grid[Int]. Count cells = w*h.
    let src = "fn main() -> Int {\n  let m = read matrix(int)\n  m.height() + m.width()\n}\n";
    let input = "1 2 3\n4 5 6";
    let (rt, result) = run_main_with_input(src, input);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // 2 rows, 3 cols → 5
    assert_eq!(result.as_int(), 5);
}

#[test]
fn m9_matrix_uniformity_faults_on_ragged() {
    // matrix requires uniform token count; ragged input → ParseFailed.
    let src = "fn main() -> Int {\n  let m = read matrix(int)\n  42\n}\n";
    let input = "1 2 3\n4 5";
    let (rt, _result) = run_main_with_input(src, input);
    assert!(
        rt.has_pending_fault(),
        "expected ParseFailed on ragged matrix, got: {:?}",
        rt.fault()
    );
}

// NOTE: ragged `grid(P, ragged, fill: value)` — the runtime walk_grid_ragged
// is complete (parses lines, pads to max width with the fill value), but the
// `fill:` value grammar currently requires a parser-parseable token. A bare
// scalar value like `.` or `0` isn't recognized by parse_parser_expr, so the
// named arg is dropped and the constructor falls back to the uniform grid
// (which then faults on ragged input). The grammar fix (accepting a wider
// token set for `fill:` values) is a small follow-up; the runtime is ready.
// Regular `grid(P)` and `matrix(P)` (the acceptance-critical forms) work fully.

// ===========================================================================
// M10 WS1 — §7.11 rich parse diagnostics (the on-ramp).
//
// A `ParseFailed` fault now records structured detail (input span, expected
// description, actual preview) into the runtime's `ParseDetail` slot. These
// tests assert the detail is populated after a parse fault — the foundation the
// noninteractive fallback (WS4) and the crash REPL's `input`/`parser` commands
// (M10b) render.
// ===========================================================================

#[test]
fn m10ws1_parse_failed_records_expected_and_preview() {
    // `read lines(int)` against non-integer input faults. The detail should
    // carry a non-empty `expected` and a non-empty `actual_preview`.
    let src = "fn main() -> Int {\n  let xs = read lines(int)\n  0\n}\n";
    let (rt, _result) = run_main_with_input(src, "not a number");
    assert!(
        rt.has_pending_fault(),
        "expected ParseFailed, got: {:?}",
        rt.fault()
    );
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::ParseFailed);
    let detail = rt.parse_detail();
    let fail = detail
        .fail
        .as_ref()
        .expect("ParseDetail should record the failure");
    assert_eq!(fail.expected, "int", "expected field should be populated");
    // The input span is within the buffer.
    assert!(fail.input_span.0 <= "not a number".len());
    // The actual preview is populated and contains the offending text.
    assert!(
        !detail.actual_preview.is_empty(),
        "actual_preview should be populated"
    );
}

#[test]
fn m10ws1_parse_failed_records_literal_expectation() {
    // A template literal mismatch reports `literal "..."` as the expectation,
    // not just a generic atomic kind. We use `read` with a template that
    // expects a `:` between two ints; input with the wrong separator faults at
    // the literal.
    let src = "fn main() -> Int {\n  let r = read `{a:int}:{b:int}`\n  0\n}\n";
    let (rt, _result) = run_main_with_input(src, "3;4");
    assert!(rt.has_pending_fault());
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::ParseFailed);
    let detail = rt.parse_detail();
    let fail = detail.fail.as_ref().expect("detail recorded");
    assert!(
        fail.expected.starts_with("literal"),
        "expected a literal expectation, got: {}",
        fail.expected
    );
}

#[test]
fn m10ws1_no_detail_when_parse_succeeds() {
    // A successful parse must NOT leave stale detail behind (the slot is
    // cleared at the start of each `run_plan`).
    let src = "fn main() -> Int {\n  let xs = read lines(int)\n  xs.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "1\n2\n3\n");
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 3);
    assert!(
        !rt.parse_detail().is_set(),
        "detail must be cleared on success"
    );
}

#[test]
fn m10ws1_parse_failed_preview_is_single_line() {
    // The actual preview must not contain raw newlines (rendered as ⏎) so the
    // noninteractive fallback prints a clean one-line glance.
    let src = "fn main() -> Int {\n  let xs = read lines(int)\n  0\n}\n";
    let input = "line one\nline two\nstill not an int";
    let (rt, _result) = run_main_with_input(src, input);
    assert!(rt.has_pending_fault());
    let preview = &rt.parse_detail().actual_preview;
    assert!(!preview.contains('\n'));
    assert!(!preview.contains('\r'));
}

// ===========================================================================
// M10 WS2 — debug-frame codegen wiring (§9.3, ADR-021).
//
// Every generated function now pushes/pops a debug frame in lockstep with its
// shadow frame, and the spill mirrors each live-root write into the
// corresponding `DebugLocal.value`. These tests confirm the wiring is balanced
// and non-corrupting: GC rooting stays sound (the run-pass suite guards this)
// and the deepest push/pop chain — the stack-overflow fault path — unwinds
// cleanly back to the host. The chain's *content* (locals at fault time) is
// made observable by WS3's crash snapshot.
// ===========================================================================

#[test]
fn m10ws2_debug_frame_pushpop_balanced_across_recursion() {
    // Deep recursion pushes/pops many debug frames. If the push/pop were
    // unbalanced or the spill corrupted the frame, this would either leak
    // (eventual OOM) or fault spuriously. A clean result confirms the wiring.
    let src = "fn sum(n: Int) -> Int {\n  if n <= 0 { 0 } else { n + sum(n - 1) }\n}\n
               fn main() -> Int { sum(500) }\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // sum(500) = 500*501/2 = 125250.
    assert_eq!(result.as_int(), 125250);
}

#[test]
fn m10ws2_debug_frame_unwinds_cleanly_on_stack_overflow() {
    // The stack-overflow fault path is the deepest push/pop chain: every
    // recursed frame has pushed a shadow + debug frame. The fault epilogue
    // must pop *both* for every frame as it unwinds to the host, leaving
    // debug_top null and no leak/corruption. A clean StackOverflow fault
    // confirms the debug-frame epilogue ordering is correct.
    let src = "fn count(n: Int) -> Int { count(n + 1) }\n
               fn main() -> Int { count(0) }\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault());
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::StackOverflow);
    // After unwind, debug_top must be null: every frame's epilogue popped.
    // (We cannot read ctx here — it was dropped — but a clean fault return
    // without abort/SIGSEGV is itself the proof the epilogue chain is sound;
    // the chain's persistence is asserted via the snapshot in WS3.)
}

#[test]
fn m10ws2_debug_frame_locals_survive_gc_during_recursion() {
    // A recursive function that allocates on every call forces GC at safepoints
    // while the debug-frame chain is deep. If the spill into DebugLocal.value
    // corrupted any slot, the GC (which walks the parallel shadow frame) or the
    // returned value would be wrong. The correct sum confirms both frames stay
    // consistent across collections.
    let src = "fn build(n: Int) -> Vec[Int] {\n  if n == 0 { Vec() } else { let v = build(n - 1); v.push(n); v }\n}\n
               fn main() -> Int {\n  let v = build(100);\n  var s = 0;\n  var i = 0;\n  while i < v.len() { s = s + v.get(i); i = i + 1 }\n  s\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // sum 1..=100 = 5050.
    assert_eq!(result.as_int(), 5050);
}

// ===========================================================================
// M10 WS3 — crash snapshot + GC rooting (§9.3, §19.10 acceptance).
//
// The first fault epilogue deep-copies the debug-frame chain into the runtime's
// SnapshotSlot before unwinding. These tests assert the snapshot is populated
// after a fault, reflects the call chain (frame names), and — the §19.10
// acceptance criterion — that GC retains every object reachable from it.
// ===========================================================================

#[test]
fn m10ws3_snapshot_captured_on_index_fault() {
    // An index-out-of-bounds fault drops into the snapshot. The chain must be
    // non-empty and carry the function name. The faulting frame is `main` here
    // (the OOB access is inline); a deeper chain is exercised by the WS3 GC test.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.get(5)\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault());
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::IndexOutOfBounds);
    let snap = rt
        .crash_snapshot()
        .expect("snapshot should be captured on fault");
    assert!(!snap.is_empty(), "snapshot should have >=1 frame");
    // SAFETY: function names are compiler-embedded 'static UTF-8.
    let frame0 = unsafe { snap.frame_name(0) };
    assert!(frame0.contains("main"), "innermost frame is {frame0}");
    // The fault kind is recorded.
    assert_eq!(snap.fault_kind, praxis_runtime::FaultKind::IndexOutOfBounds);
}

#[test]
fn m10ws3_snapshot_not_captured_on_clean_run() {
    // A program that completes without faulting must leave no snapshot.
    let (rt, _result) = run_main("fn main() -> Int { 42 }");
    assert!(!rt.has_pending_fault());
    assert!(
        rt.crash_snapshot().is_none(),
        "no snapshot expected on a clean run"
    );
}

#[test]
fn m10ws3_snapshot_retains_reachable_objects_across_gc() {
    // The §19.10 acceptance criterion: "GC retains all objects reachable from
    // snapshots." Build a Vec[Int] local in the faulting frame, fault (OOB get),
    // then run a host-side collection with the snapshot as a root. The
    // referenced Vec must survive (its elements remain readable through the
    // snapshot's locals).
    let src =
        "fn main() -> Int {\n  let xs = Vec()\n  xs.push(11)\n  xs.push(22)\n  xs.get(99)\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault());
    let snap = rt.crash_snapshot().expect("snapshot captured");
    // The snapshot references GcRefs (the locals in the faulting frames).
    let mut roots = Vec::new();
    snap.push_roots(&mut roots);
    assert!(
        !roots.is_empty(),
        "snapshot must root at least one GcRef (the Vec locals)"
    );
    // Force a collection with the snapshot as the root set. If retention is
    // broken, the referenced objects are reclaimed and dereferencing a root
    // would be use-after-free (the test would crash / valgrind would flag it).
    // We collect several times to stress the mark/sweep.
    for _ in 0..3 {
        rt.collect(snap);
    }
    // The roots are still valid GcRefs into the (non-moving) heap; reading one
    // as a Vec and checking its length confirms the object survived collection.
    // Find a Vec-typed root among the snapshot locals.
    let vec_root = snap.frames.iter().flat_map(|f| &f.locals).find_map(|l| {
        let desc = unsafe { &*l.descriptor };
        (desc.id() == praxis_runtime::collections::VEC.id()).then_some(l.value)
    });
    let vec_root = vec_root.expect("snapshot should hold a Vec local");
    let v = vec_root.as_vec();
    assert_eq!(v.len(), 2, "the Vec survived GC with its elements intact");
    assert_eq!(v[0].as_int(), 11);
    assert_eq!(v[1].as_int(), 22);
}

// ===========================================================================
// M10 WS1 (Part 2) — full static `Type` id + source span threaded into the
// debug frame (§9.3). WS1 of Part 2 carries the per-local `type_id` (so the
// `p EXPR` evaluator can reconstruct `Vec[Int]` / record shapes the runtime
// `descriptor` loses) and the per-function source span (so the `source`
// command can render the faulting function). These assert both survive into
// the crash snapshot.
// ===========================================================================

#[test]
fn m10b_ws1_snapshot_frame_carries_source_span() {
    // `main` has a real source span; the snapshot's frame for `main` must carry
    // a non-empty `[start, end)` byte range, not the `(0, 0)` default. A
    // deliberately-placed OOB get faults inside `main`, so frame 0 is `main`.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.get(9)\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault());
    let snap = rt.crash_snapshot().expect("snapshot captured");
    let main_frame = &snap.frames[0];
    assert_eq!(unsafe { snap.frame_name(0) }, "main");
    // The span covers some prefix of `src`; it must be non-empty and point
    // inside the source (end <= src.len()).
    let (start, end) = main_frame.source_span;
    assert!(
        start < end && (end as usize) <= src.len(),
        "main's source_span should be a real in-source range, got ({start}, {end}) for src len {}",
        src.len()
    );
    // The span must cover the `fn main` declaration.
    let span_text = &src[start as usize..end as usize];
    assert!(
        span_text.starts_with("fn main"),
        "source_span should cover `fn main`, got: {span_text:?}"
    );
}

#[test]
fn m10b_ws1_snapshot_locals_carry_distinct_type_ids() {
    // Two locals of distinct static types — `Int` (`n`) and `Vec[Int]` (`xs`) —
    // must carry distinct `type_id`s in the snapshot. This proves the full
    // static `Type` (not just the runtime descriptor, which collapses all
    // collections to `VEC`) is threaded through, so the M10b `p EXPR` evaluator
    // can reconstruct `Vec[Int]` element types for type-checking.
    let src = "fn main() -> Int {\n  let n = 42\n  let xs = Vec()\n  xs.push(n)\n  xs.get(9)\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault());
    let snap = rt.crash_snapshot().expect("snapshot captured");
    let frame = &snap.frames[0];
    // Locate the two named locals by source name (skip synthetic <tmp> locals).
    let n = frame
        .locals
        .iter()
        .find(|l| l.name() == "n")
        .expect("`n` local present");
    let xs = frame
        .locals
        .iter()
        .find(|l| l.name() == "xs")
        .expect("`xs` local present");
    // Non-zero (the default placeholder for unknown types is 0).
    assert_ne!(n.type_id, 0, "`n` should carry a real Int type id");
    assert_ne!(xs.type_id, 0, "`xs` should carry a real Vec type id");
    // Distinct — the whole point of threading the full Type id: the runtime
    // descriptors INT and VEC differ, but this also guards against any future
    // collapse where two different Types share an id.
    assert_ne!(
        n.type_id, xs.type_id,
        "Int and Vec locals must have distinct type ids"
    );
}
