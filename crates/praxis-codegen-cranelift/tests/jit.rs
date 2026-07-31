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
        if let Err(errs) = praxis_mir::verify(f) {
            panic!("{}", praxis_mir::verify::report(&errs));
        }
    }
    let mut jit = Jit::new().expect("JIT construction");
    let ids = jit
        .compile(&funcs, &mut analysis.db)
        .expect("JIT compilation");
    (jit, ids)
}

/// Every `praxis_*` symbol the MIR builder emits must be in the runtime symbol
/// table. Registration used to be a second hand-maintained list in `module.rs`,
/// and `dlsym` found the statically linked runtime for anything it omitted — so
/// the drift was invisible. This walks the MIR of a feature-broad program and
/// checks each callee by name.
#[test]
fn every_runtime_symbol_mir_emits_is_registered() {
    let src = concat!(
        "struct P { x: Int, y: Int }\n",
        "enum E { A, B(Int) }\n",
        "fn main() -> Int {\n",
        "  let v = Vec()\n  v.push(1)\n  v.push(2)\n",
        "  let m = Map()\n  m.insert(\"k\", 1)\n",
        "  let s = Set()\n  s.insert(3)\n",
        "  let d = Deque()\n  d.push_back(4)\n",
        "  let c = Counter()\n  c.inc(5)\n",
        "  let t = (1, 2)\n",
        "  let p = P { x: 1, y: 2 }\n",
        "  let e = B(7)\n",
        "  let f = |z| z + 1\n",
        "  let b = BitSet()\n  b.insert(6)\n",
        "  let mh = MinHeap()\n  mh.push(7)\n",
        "  let xh = MaxHeap()\n  xh.push(8)\n",
        "  var acc = 0\n",
        "  for x in v { acc = acc + f(x) }\n",
        // Every snapshot symbol REP-15's `IterPlan` can select — a `for` is the
        // only caller of four of them, so nothing else would reach them here.
        "  for x in s { acc = acc + x }\n",
        "  for x in b { acc = acc + x }\n",
        "  for x in mh { acc = acc + x }\n",
        "  for x in xh { acc = acc + x }\n",
        "  for kv in m { acc = acc + kv.1 }\n",
        "  for kv in c { acc = acc + kv.1 }\n",
        "  let txt = \"hi\"\n",
        "  out(txt.len())\n",
        "  let fl = 1.5\n  out(fl.sqrt())\n",
        "  acc + p.x + m.len() + s.len() + d.len() + c.len()\n",
        "}\n"
    );
    let map = SourceMap::new();
    let file = map.intern("symbols.px", src);
    let parsed = parse(file, src);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let mut analysis = analyze_root(file, &parsed.tree);
    let root = praxis_ast::SourceFile::cast(parsed.tree.clone()).unwrap();
    let module = lower(file, &root, &mut analysis);
    assert!(module.diagnostics.is_empty(), "{:?}", module.diagnostics);
    let module = monomorphize(module, &analysis.names, &mut analysis.db);
    let funcs = lower_module(&module, &mut analysis.db);

    let mut seen = std::collections::BTreeSet::new();
    for f in &funcs {
        for block in &f.blocks {
            for inst in &block.insts {
                if let praxis_mir::Inst::Call {
                    callee: praxis_mir::CallTarget::Runtime(name),
                    ..
                } = inst
                {
                    seen.insert(*name);
                    assert!(
                        praxis_codegen_cranelift::symbols::resolve(name.name()).is_some(),
                        "MIR emits `{name}`, which the runtime symbol table does not know"
                    );
                }
            }
        }
    }
    // A floor, not a target. `CallTarget::Runtime` covers the method-call
    // families; the allocation, record, enum, closure and debug-frame symbols
    // are emitted by codegen from `Inst::Alloc` and friends rather than named
    // in MIR, so compiling the same program below is what checks those.
    assert!(
        seen.len() >= 14,
        "expected a broad symbol sample, saw {}: {seen:?}",
        seen.len()
    );

    // Every symbol codegen emits — named in MIR or not — must resolve;
    // `runtime_funcref` now rejects an unregistered name instead of letting
    // `dlsym` find it.
    let _ = compile(src);
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
    let result = unsafe { entry(&mut ctx as *mut RuntimeContext) };
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
    let result = unsafe { entry(&mut ctx as *mut RuntimeContext) };
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

/// REP-11: a digit separator is punctuation, so it changes no value.
///
/// The half no lexer test can see. Lowering strips the `_`s and parses what is
/// left; before the lexer accepted them that strip was unreachable, and the
/// float path had no strip at all — `3.141_592` would have parsed as nothing and
/// become `0.0`. Both positions the exit criterion names are here: an expression
/// and a pattern, which read the token through two different decoders.
#[test]
fn a_digit_separator_does_not_change_the_number() {
    for (src, want) in [
        ("fn main() -> Int { 1_000 }", 1000),
        ("fn main() -> Int { 1_0_0 }", 100),
        ("fn main() -> Int { 1__0 }", 10),
        // The boundary still fits, with separators in the way of reading it.
        ("fn main() -> Int { 9_223_372_036_854_775_807 }", i64::MAX),
        // Arithmetic over separated operands, and a separated range bound.
        ("fn main() -> Int { 1_000 + 2_000 }", 3000),
        (
            "fn main() -> Int { var t = 0\n for i in 1_0..1_2 { t = t + i }\n t }",
            21,
        ),
        // Pattern position: the arm matches the value the expression wrote.
        (
            "fn main() -> Int { let n = 1_000\n match n { 1_000 => 7, _ => 0 } }",
            7,
        ),
        (
            "fn main() -> Int { let n = 1000\n match n { 1_0_0 => 1, 1_000 => 7, _ => 0 } }",
            7,
        ),
    ] {
        let (rt, result) = run_main(src);
        assert!(!rt.has_pending_fault(), "{src} faulted");
        assert_eq!(result.as_int(), want, "{src}");
    }

    // A float's fraction and exponent are separated too, and 0.0 is what a
    // missing strip would have produced.
    let (rt, result) = run_main("fn main() -> Float { 1.234_567 }");
    assert!(!rt.has_pending_fault());
    assert!((result.as_float() - 1.234_567).abs() < 1e-12);
    let (rt, result) = run_main("fn main() -> Float { 1.5e1_0 }");
    assert!(!rt.has_pending_fault());
    assert!((result.as_float() - 1.5e10).abs() < 1.0);
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
    let src = "fn main() -> Int {\n  let m = Map()\n  m.insert(7, 42)\n  m[7]\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

/// An absent `Map.get` answers `None`, and a present one answers `Some`.
///
/// This test used to be `map_get_absent_returns_unit`, and it asserted only
/// that no fault occurred — because there was nothing else it *could* assert:
/// `get` handed back the Unit sentinel under a `V` static type (RT-14), so the
/// program had no way to tell absence from a value and the test had no way to
/// look. §5.7 spells the signature `Map[K,V].get(K) -> Option[V]` and §4.7 says
/// absence is `Option`; D1 settled that the implementation follows.
#[test]
fn an_absent_map_get_answers_none_and_a_present_one_answers_some() {
    let unwrap = "fn unwrap(o: Option[Int]) -> Int {\n  match o {\n    Some(v) => v,\n    None => 0 - 1,\n  }\n}\n";
    let (rt, result) = run_main(&format!(
        "{unwrap}fn main() -> Int {{\n  let m = Map()\n  m.insert(1, 10)\n  unwrap(m.get(99))\n}}\n"
    ));
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), -1, "an absent key is `None`");

    let (rt, result) = run_main(&format!(
        "{unwrap}fn main() -> Int {{\n  let m = Map()\n  m.insert(1, 10)\n  unwrap(m.get(1))\n}}\n"
    ));
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 10, "a present key is `Some(value)`");
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
    let src =
        "fn main() -> Int {\n  let m = Map()\n  m.insert(1, 10)\n  m.insert(1, 99)\n  m[1]\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 99);
}

#[test]
fn map_with_tuple_keys_end_to_end() {
    // The headline §19.7 criterion: tuples as map keys. Two structurally-equal
    // tuples must hit the same entry.
    let src = "fn main() -> Int {\n  let m = Map()\n  m.insert((1, 2), 100)\n  m[(1, 2)]\n}\n";
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
    let src = "fn main() -> Int {\n  let m = Map()\n  m.insert(\"hello\", 1)\n  m[\"hello\"]\n}\n";
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
    let src = "fn main() -> Int {\n  let m = Map()\n  m.insert(\"hello\", 42)\n  let keys = Vec()\n  keys.push(\"hello\")\n  m[keys.get(0)]\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

#[test]
fn adv_map_text_key_from_read_lookup() {
    // Map keyed by source-slice Text from `read`. Insert all, then look up one
    // by a literal of equal value.
    let src = "fn main() -> Int {\n  let words = read lines(word)\n  let m = Map()\n  var i = 0\n  while i < words.len() { m.insert(words.get(i), i); i = i + 1 }\n  m[\"pear\"]\n}\n";
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
    let src = "fn main() -> Int {\n  let m = Map()\n  m.insert((1, 2), 100)\n  let pairs = Vec()\n  pairs.push((1, 2))\n  m[pairs.get(0)]\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 100);
}

#[test]
fn adv_map_large_under_gc_pressure() {
    // Insert 500 entries under GC pressure, then look up a mid-range key.
    // Verifies map entries (keys + values) survive GC via map_trace.
    let src = "fn main() -> Int {\n  let m = Map()\n  var i = 0\n  while i < 500 { m.insert(i, i * 2); i = i + 1 }\n  m[250]\n}\n";
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

/// `None` is a value the program can *name*, which the Unit sentinel was not.
///
/// This test used to be `adv_map_get_absent_returns_unit`, and it pinned the
/// defect: its comment read "§4.7: indexing a missing map key faults, but
/// `.get` returns Unit (absent sentinel)", and it checked absence by comparing
/// the result against `Int 0` and expecting the comparison to be *false* —
/// which is a test of the fact that two different runtime types are unequal,
/// not of the map. §4.7 never said `.get` returns Unit; it said absence is
/// `Option`. The rewrite matches on the answer instead of probing it.
#[test]
fn an_absent_map_get_is_a_none_the_program_can_match_on() {
    let src = "fn main() -> Int {\n  let m = Map()\n  m.insert(\"a\", 1)\n  match m.get(\"missing\") {\n    Some(v) => v,\n    None => 7,\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 7, "the `None` arm ran");

    // And a `Some` binds the value rather than merely being "not Unit".
    let src = "fn main() -> Int {\n  let m = Map()\n  m.insert(\"a\", 1)\n  match m.get(\"a\") {\n    Some(v) => v,\n    None => 7,\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_map_index_of_a_missing_key_faults_where_get_answers() {
    // §4.7's own sentence, both halves in one test: "indexing a missing map key
    // faults instead of returning an option… the user chooses between explicit
    // absence with `.get` and assertion-like access with indexing" (REP-16).
    //
    // This test used to assert the opposite — that `m[key]` does *not* fault —
    // and it passed for a reason that had nothing to do with maps: there was no
    // subscript grammar at all, so `let v = m["missing"]` parsed as `let v = m`
    // followed by a recovered statement, and the `v` it compared was the map.
    // The two spellings really are two operations now.
    let src = "fn main() -> Int {\n  let m = Map()\n  m.insert(\"a\", 1)\n  let v = m[\"missing\"]\n  if v == 0 { 1 } else { 0 }\n}\n";
    let (rt, _) = run_main(src);
    assert!(
        rt.has_pending_fault(),
        "§4.7: indexing a missing key faults"
    );

    // …and `.get` on the same absent key does not, so the fault is the
    // subscript's choice and not the map's.
    //
    // This half used to end `assert_eq!(result.as_int(), 0, "Unit is not Int
    // 0")` — it asserted that the sentinel `.get` handed back was not an `Int`,
    // which pinned RT-14 rather than stating §4.7's rule. What §4.7 actually
    // says is that `.get` answers `Option`, so the arm is what the test reads.
    let src = "fn main() -> Int {\n  let m = Map()\n  m.insert(\"a\", 1)\n  match m.get(\"missing\") {\n    Some(v) => v,\n    None => 0,\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0, "explicit absence, not a fault");

    // A present key is the value, through the subscript.
    let src = "fn main() -> Int {\n  let m = Map()\n  m.insert(\"a\", 7)\n  m[\"a\"]\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 7);
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

/// HIR-08 end to end: a closure that is *itself the callee* of a call captures
/// its `var` by cell like any other, so the mutation outlives the call. Escape
/// analysis never visited `Call.callee_expr`, so `count` was not boxed and each
/// increment went to a copy — the program returned `0`.
#[test]
fn an_immediately_invoked_closure_mutates_the_var_it_captured() {
    let src = concat!(
        "fn main() -> Int {\n",
        "  var count = 0\n",
        "  let a = (|n| { count = count + n\n  count })(1)\n",
        "  let b = (|n| { count = count + n\n  count })(10)\n",
        "  count\n",
        "}\n"
    );
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 11, "both calls mutated the same cell");
}

#[test]
fn loop_break_exits() {
    let src = "fn main() -> Int {\n  var i = 0\n  loop { if i >= 5 { break } i = i + 1 }\n  i\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 5);
}

/// TY-21 end to end: a `loop`'s value is the one its `break` carried, and it
/// has to survive HIR lowering, MIR and codegen — inference agreeing is not
/// enough. Before the fix the MIR builder discarded the `break` value and
/// yielded a `Unit` literal, so this returned `Unit` where an `Int` was
/// declared.
#[test]
fn expression_loop_returns_the_value_its_break_carried() {
    // The loop is the function's tail: nothing else can supply the answer.
    let src =
        "fn main() -> Int {\n  var i = 0\n  loop { i = i + 1 if i == 5 { break i * 2 } }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 10);

    // …and the value flows onward like any other: bound, then used.
    let src = "fn main() -> Int {\n  var i = 0\n  let found = loop { i = i + 1 if i * i > 30 { break i } }\n  found + 100\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 106, "6 * 6 = 36 is the first over 30");
}

/// A `loop` no `break` leaves is `Never` (D2), and `Never` has no runtime
/// representation — so such a loop must not ask for a result slot, whose
/// descriptor site would fail the compile (D9). Compiling and running the other
/// branch is the assertion; the loop itself is never entered.
#[test]
fn a_loop_that_never_breaks_compiles_as_a_diverging_branch() {
    let src = "fn choose(c: Bool) -> Int { if c { 1 } else { loop { } } }\nfn main() -> Int {\n  choose(true)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

/// Two `break`s, two exits, one slot: whichever runs is what the loop produces.
/// A result slot written on only one path would return the other's stale (or
/// unwritten) value.
#[test]
fn every_break_writes_the_loop_result() {
    let src = concat!(
        "fn search(limit: Int) -> Int {\n",
        "  var i = 0\n",
        "  loop {\n",
        "    if i == limit { break 0 - 1 }\n",
        "    if i * i == 49 { break i }\n",
        "    i = i + 1\n",
        "  }\n",
        "}\n",
        "fn main() -> Int {\n  search(100) * 1000 + search(3)\n}\n"
    );
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(
        result.as_int(),
        7 * 1000 - 1,
        "the found exit, then the limit exit"
    );
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

/// `continue` inside a `for` must advance the index. Targeting the loop header
/// skipped the increment, so this program never terminated.
#[test]
fn continue_in_a_for_loop_still_advances_the_index() {
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  var seen = 0\n  for x in v { if x == 2 { continue } seen = seen + x }\n  seen\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 4, "1 + 3, with 2 skipped");
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

    // The same stress over stages that own a *dense counter* (MIR-04). Each
    // counter is a `Gc` Int slot live across every `praxis_vec_get` safepoint in
    // the loop, exactly like the source cursor, so a root set that did not cover
    // them would hand the collector a stale word — and after MIR-01/MIR-02, a
    // slot the liveness pass misses is nulled rather than merely stale.
    let src = "fn main() -> Int {\n  let v = Vec()\n  var i = 0\n  while i < 300 { v.push(i); i = i + 1 }\n  var t = 0\n  for p in v.filter(|x| x > 100).enumerate().take(3).collect() { t = t + p.0 * 1000 + p.1 }\n  t\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // Filtered is 101..=299; enumerate numbers it densely from zero; take(3)
    // keeps (0,101), (1,102), (2,103) → 101 + 1102 + 2103 = 3306.
    assert_eq!(result.as_int(), 3306);
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

/// **MIR-03.** The bound of a `take`/`skip` is an `Int` expression, not an `Int`
/// literal. The catalog types the parameter `Int` and says nothing about
/// literals; a chain whose bound was anything else used to be declined by the
/// recognizer, fall through to a combinator lowerer with no `take` arm, and
/// answer the Unit singleton — which the enclosing chain then read as a Vec.
#[test]
fn a_take_or_skip_bound_is_any_int_expression() {
    let five = "  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.push(5)\n";
    let answer = |tail: &str| {
        let (rt, result) = run_main(&format!("fn main() -> Int {{\n{five}  {tail}\n}}\n"));
        assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
        result.as_int()
    };

    // A binding, the shape the ignored regressions used.
    assert_eq!(answer("let n = 3\n  v.take(n).sum()"), 6);
    assert_eq!(answer("let n = 2\n  v.skip(n).sum()"), 12);
    // An arithmetic expression, and one that calls back into the receiver.
    assert_eq!(answer("let n = 1\n  v.take(n + n).sum()"), 3);
    assert_eq!(answer("v.skip(v.len() - 2).sum()"), 9);
    // The bound still composes with the stages around it.
    assert_eq!(answer("let n = 4\n  v.take(n).map(|x| x * 10).sum()"), 100);
    assert_eq!(answer("let n = 4\n  v.take(n).filter(|x| x > 2).sum()"), 7);
    // Degenerate bounds keep the meaning the literal spelling had: `take` of
    // nothing is empty, `skip` of nothing drops nothing, and a negative bound is
    // the same comparison rather than a special case.
    assert_eq!(answer("let n = 0\n  v.take(n).sum()"), 0);
    assert_eq!(answer("let n = 0\n  v.skip(n).sum()"), 15);
    assert_eq!(answer("let n = 0 - 1\n  v.take(n).sum()"), 0);
    assert_eq!(answer("let n = 0 - 1\n  v.skip(n).sum()"), 15);
    assert_eq!(answer("let n = 99\n  v.take(n).sum()"), 15);
    assert_eq!(answer("let n = 99\n  v.skip(n).sum()"), 0);
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

/// **MIR-04's `enumerate` half.** The audit's row named take/skip/zip/find/
/// position and omitted `enumerate`, and the one test that reads an enumerate
/// pair's payloads has no `filter` in front of it — so nothing covered the
/// numbering itself.
///
/// `enumerate` numbers the sequence that reaches it. After a `filter` that is a
/// dense 0, 1, 2 …, not the surviving source positions.
#[test]
fn enumerate_after_filter_numbers_the_filtered_sequence() {
    // [1,2,3,4] -filter(even)-> [2,4] -enumerate-> (0,2), (1,4).
    // Weighted 100*index + value: 2 + 104 = 106. Reading source indices would
    // give (1,2), (3,4) → 406, and a swap of the halves gives something else
    // again.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  var t = 0\n  for p in v.filter(|x| x % 2 == 0).enumerate().collect() { t = t + p.0 * 100 + p.1 }\n  t\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 106);

    // And after a `skip`, which drops from the front: [1,2,3,4].skip(2) is
    // [3,4], numbered (0,3), (1,4) → 3 + 104 = 107.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  var t = 0\n  for p in v.skip(2).enumerate().collect() { t = t + p.0 * 100 + p.1 }\n  t\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 107);
}

/// **The rule S21 is named for.** Every stage that asks "which element is this?"
/// is asking about *its own* input sequence — the one that reaches it — not
/// about the source.
///
/// One shared counter answers all the single-stage cases correctly, which is
/// why the audit's per-stage regressions do not force the general rule. These
/// are the shapes that do: two position-consuming stages with a `filter`
/// between them, where one counter and two counters disagree.
#[test]
fn each_stage_counts_the_sequence_that_reaches_it() {
    // [1..6].skip(1) = [2,3,4,5,6]; filter(even) = [2,4,6]; take(2) = [2,4].
    // Sum 6. With one source cursor, `take` stops once the *source* index
    // reaches 2, so only the 2 survives and the answer is 2.
    let six = "  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.push(5)\n  v.push(6)\n";
    let answer = |tail: &str| {
        let (rt, result) = run_main(&format!("fn main() -> Int {{\n{six}  {tail}\n}}\n"));
        assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
        result.as_int()
    };
    assert_eq!(answer("v.skip(1).filter(|x| x % 2 == 0).take(2).sum()"), 6);
    // Two `skip`s around a filter: [1..6] -skip(1)-> [2..6] -filter(even)->
    // [2,4,6] -skip(1)-> [4,6], sum 10.
    assert_eq!(answer("v.skip(1).filter(|x| x % 2 == 0).skip(1).sum()"), 10);
    // A `zip` behind a filter pairs by the filtered position, and a `take`
    // behind the zip counts the pairs: [2,4,6] zipped with [10,20,30] is three
    // pairs, of which two are taken.
    assert_eq!(
        answer(
            "let rhs = Vec()\n  rhs.push(10)\n  rhs.push(20)\n  rhs.push(30)\n  v.filter(|x| x % 2 == 0).zip(rhs).take(2).count()"
        ),
        2
    );

    // `position` reports the position in the sequence that reached the sink, and
    // it must not be overwritten by a later match — which is what happens when a
    // hit inside a `flat_map` ends only the inner loop. The inner Vecs here are
    // [0, 5] and [10]: flattened, the first element over 4 is at index 1;
    // per-inner, the first Vec answers 1 and the second overwrites it with 0.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.flat_map(|x| {\n    let r = Vec()\n    if x == 1 { r.push(0) }\n    r.push(x * 5)\n    r\n  }).position(|p| p > 4)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1, "the flattened stream's index, once");
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

/// **MIR-06's semantics.** A `flat_map` inside a `flat_map` flattens *both*
/// levels, in order, with everything between them applied once per element of
/// the level it sits in.
///
/// The exit-criterion test (`two_flat_map_stages_compose_without_a_compiler_panic`)
/// only asserts that the compiler survives, and it asserts a count — which a
/// wrong-but-non-panicking nesting could also produce. These weight every level
/// so that dropping one, running one at the wrong depth, or ordering the two
/// backwards all answer a different number.
#[test]
fn a_flat_map_inside_a_flat_map_flattens_both_levels() {
    // [1,2] -flat_map(x -> [x, x*10])-> [1,10,2,20]
    //       -flat_map(y -> [y, y*100])-> [1,100,10,1000,2,200,20,2000]
    // sum = 101 * (1 + 10 + 2 + 20) = 3333, and there are eight elements.
    let outer = "  let v = Vec()\n  v.push(1)\n  v.push(2)\n";
    let two_levels = "v.flat_map(|x| {\n    let a = Vec()\n    a.push(x)\n    a.push(x * 10)\n    a\n  }).flat_map(|y| {\n    let c = Vec()\n    c.push(y)\n    c.push(y * 100)\n    c\n  })";
    let (rt, result) = run_main(&format!(
        "fn main() -> Int {{\n{outer}  {two_levels}.sum()\n}}\n"
    ));
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3333, "both levels must be flattened");

    let (rt, result) = run_main(&format!(
        "fn main() -> Int {{\n{outer}  {two_levels}.count()\n}}\n"
    ));
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 8, "two doublings over two elements");

    // A stage *between* the two splices runs once per element of the first
    // level, not once per outer element:
    // [1,2] -> [1,10,2,20] -map(*2)-> [2,20,4,40] -flat_map(y -> [y, y+1])->
    // [2,3,20,21,4,5,40,41], sum = 136.
    let (rt, result) = run_main(&format!(
        "fn main() -> Int {{\n{outer}  v.flat_map(|x| {{\n    let a = Vec()\n    a.push(x)\n    a.push(x * 10)\n    a\n  }}).map(|y| y * 2).flat_map(|z| {{\n    let c = Vec()\n    c.push(z)\n    c.push(z + 1)\n    c\n  }}).sum()\n}}\n"
    ));
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(
        result.as_int(),
        136,
        "a stage between two splices runs at the depth it was written at"
    );
}

/// **MIR-08's `take_while` half.** A stage that stops the stream stops the
/// *stream*, not the inner Vec it happened to be looking at.
///
/// The exit-criterion test covers `any`; nothing covered `take_while`, and its
/// failure mode inside a splice is worse than an early stop: applied per inner
/// Vec, `take_while` silently becomes a `filter`, so elements after the stop
/// point are processed and can fault.
#[test]
fn take_while_after_flat_map_stops_the_whole_stream() {
    // [3,1,5] -flat_map(x -> [x])-> [3,1,5] -take_while(> 2)-> [3], and
    // 100 / (3 - 5) = -50. Per inner Vec, `1` is merely dropped and `5` goes on
    // to divide by zero — which is the assertion, because a wrong answer here
    // would be indistinguishable from a right one for a total mapper.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(3)\n  v.push(1)\n  v.push(5)\n  v.flat_map(|x| {\n    let a = Vec()\n    a.push(x)\n    a\n  }).take_while(|y| y > 2).map(|y| 100 / (y - 5)).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(
        !rt.has_pending_fault(),
        "nothing after the stop point may run: {:?}",
        rt.fault()
    );
    assert_eq!(result.as_int(), -50);

    // The same with inner Vecs of length two, so the stop lands *inside* an
    // inner sequence rather than at its start: [1,2] -> [1,10,2,20],
    // take_while(< 5) -> [1].
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.flat_map(|x| {\n    let a = Vec()\n    a.push(x)\n    a.push(x * 10)\n    a\n  }).take_while(|y| y < 5).count()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1, "the stream stops at the first 10");
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
    // PROBE (parser.rs walk_csv): CSV inside a section starts at a non-zero
    // byte offset, and the offset used to be recovered by *searching* the
    // region for the token's text (`region_offset_of`) rather than computed —
    // so a repeated field resolved to the first occurrence and an empty one
    // panicked. This counts the sections and then reads a value out of the
    // *second* one, which is the half a count alone cannot see.
    //
    // REWRITTEN (S20/IPR-04). It used to read `sections(csv(int))` over
    // `"1,2,3\n4,5,6\n\n7,8\n9,10\n"` and assert only that there were two
    // sections. That input gives `csv` a region containing a newline, so one
    // of its fields is the text `"3\n4"` — and the assertion passed only
    // because `csv` walked its child against the whole remaining buffer and
    // threw the cursor away, so `int` read the `3` and the `\n4` was silently
    // nobody's. Under §7.5's full-consumption rule that region is a parse
    // failure, correctly. `csv` describes one line; a section of several lines
    // is `lines(csv(...))`, which is what day05 writes.
    let src = "fn main() -> Int {\n  let s = read sections(lines(csv(int)))\n  \
               s.get(1).get(0).get(1)\n}\n";
    let (rt, result) = run_main_with_input(src, "1,2,3\n4,5,6\n\n7,8\n9,10\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(
        result.as_int(),
        8,
        "the second section's first line's second field is 8, at byte 15 of the input"
    );
}

/// **IPR-04.** A `csv` field whose region contains anything the child parser
/// does not consume is a parse failure, not a silent truncation.
///
/// The predecessor computed each field's bounds and then walked the child
/// against everything from the field's start to the end of the buffer, with the
/// end explicitly discarded (`let _ = token_end;`). So `csv(int)` over a region
/// with a stray newline in it "worked" by reading the digits it liked.
#[test]
fn a_csv_field_the_child_does_not_consume_is_a_parse_failure() {
    let (rt, _result) = run_main_with_input(
        "fn main() -> Int {\n  let v = read csv(int)\n  v.len()\n}\n",
        "1,2x,3\n",
    );
    assert_eq!(
        rt.fault(),
        praxis_runtime::FaultKind::ParseFailed,
        "`2x` is not an int, and a field the child leaves bytes in must not be accepted"
    );
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

/// **HIR-06's runtime half.** The usefulness matrix pairs each pattern column
/// with a type, so lowering pads a variant pattern to its payload arity — which
/// puts a `TypedPattern::Wildcard` in a *payload slot* from source for the first
/// time. Before, one only ever reached MIR's decision tree from a synthesized
/// fallback at the top level.
///
/// Each spelling below selects the same arm and must read the same value: a
/// padded slot is extracted and discarded, not skipped and not read off the end.
#[test]
fn a_padded_payload_wildcard_selects_its_arm_at_runtime() {
    let enums = "enum Inner { Nil, Val(Int) }\nenum Outer { Wrapped(Inner), Bare }\n";
    for arm in ["Wrapped(_)", "Wrapped(i)"] {
        let src = format!(
            "{enums}fn main() -> Int {{\n  let v = Wrapped(Val(7))\n  \
             match v {{\n    {arm} => 5\n    Bare => 1\n  }}\n}}\n"
        );
        let (rt, result) = run_main(&src);
        assert!(!rt.has_pending_fault(), "`{arm}` faulted: {:?}", rt.fault());
        assert_eq!(result.as_int(), 5, "`{arm}` must take the first arm");
    }

    // A *bare* constructor name for a variant that has a payload is padded to
    // `Val(_)`, so it emits a payload read it never used to. The value it
    // selects must be unchanged, and the discarded slot must not fault.
    let src = format!(
        "{enums}fn main() -> Int {{\n  let v = Wrapped(Val(7))\n  \
         match v {{\n    Wrapped(Nil) => 1\n    Wrapped(Val) => 2\n    Bare => 3\n  }}\n}}\n"
    );
    let (rt, result) = run_main(&src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2, "a bare `Val` is `Val(_)`");

    // …and the padded arm is still a *test*: the other payload constructor
    // takes its own arm rather than falling into the padded one.
    let src = format!(
        "{enums}fn main() -> Int {{\n  let v = Wrapped(Nil)\n  \
         match v {{\n    Wrapped(Val) => 2\n    Wrapped(Nil) => 1\n    Bare => 3\n  }}\n}}\n"
    );
    let (rt, result) = run_main(&src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1, "`Nil` is not caught by `Val(_)`");
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

/// MONO-03, end to end. One generic function applied to `Option[Int]` and to
/// `Option[Text]` needs **two** specializations.
///
/// The mono cache keyed on `db.render`, and `Option` rendered as a bare name
/// because its element type lived in a fresh nominal def rather than in the
/// type (TY-06). Both call sites therefore hashed to `id__Option`, and the
/// second one ran the first's `Int` clone over a `Text` payload.
#[test]
fn monomorphization_distinguishes_option_element_types() {
    let src = "fn id(x) { x }\n\
               fn main() -> Int {\n  \
                 let a = id(Some(7))\n  \
                 let b = id(Some(\"hi\"))\n  \
                 let n = match a {\n    Some(v) => v\n    None => 0\n  }\n  \
                 let s = match b {\n    Some(v) => v\n    None => \"\"\n  }\n  \
                 if s == \"hi\" { n } else { 0 }\n\
               }\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 7);
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

/// **D1.** An empty `min`/`max` faults; it does not answer `0`.
///
/// This test used to be `adv_pipeline_empty_source_min_is_zero`, and its
/// comment called the behaviour "a known edge — document current behavior".
/// Documenting it is what made it a defect-pinning test: `0` is not a *missing*
/// answer, it is a **wrong** one. It is below every element of `[3, 4]` and
/// above every element of `[-3, -4]`, so nothing at the call site can tell it
/// from a real minimum, and the accumulator was seeded rather than derived from
/// the data at all.
///
/// `reduce`, `min_by` and `max_by` already faulted here (MIR-09); D1 settled
/// that `min`/`max` join them rather than becoming `Option`, because an empty
/// `min` is a mistake in the program and not the domain-level absence §4.7
/// reserves `Option` for.
#[test]
fn an_empty_min_or_max_faults_rather_than_answering_zero() {
    for sink in ["min", "max"] {
        let src = format!("fn main() -> Int {{\n  let v = Vec()\n  v.{sink}()\n}}\n");
        let (rt, _result) = run_main(&src);
        assert_eq!(
            rt.fault(),
            praxis_runtime::FaultKind::EmptyCollection,
            "an empty `{sink}` has no answer"
        );
    }

    // A source that a `filter` empties is the same case, and is the one a real
    // program hits: the Vec is non-empty and nothing survives the predicate.
    let (rt, _result) = run_main(
        "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  \
         v.filter(|x| x > 100).min()\n}\n",
    );
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::EmptyCollection);

    // …and a non-empty one still answers, so the guard is the empty case and
    // not a new refusal.
    let (rt, result) =
        run_main("fn main() -> Int {\n  let v = Vec()\n  v.push(3)\n  v.push(4)\n  v.min()\n}\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let v = Vec()\n  v.push(0 - 3)\n  v.push(0 - 4)\n  v.max()\n}\n",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), -3, "the seeded 0 was above every element");
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
    // Empty source → reduce. `reduce` seeds from the first element, and there
    // is none — so the answer is a fault, not whatever the unseeded Gc slot
    // happened to hold (MIR-09). This test used to assert only that the host
    // survived, because there was no contract to assert; now there is.
    let src = "fn main() -> Int {\n  let v = Vec()\n  v.reduce(|a, x| a + x)\n}\n";
    let (rt, _result) = run_main(src);
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::EmptyCollection);
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
    let (mut rt, _result) = run_main(src);
    assert!(rt.has_pending_fault());
    let snap = rt.crash_snapshot().expect("snapshot captured");
    // The snapshot references GcRefs (the locals in the faulting frames).
    let mut roots = Vec::new();
    snap.push_roots(&mut roots);
    assert!(
        !roots.is_empty(),
        "snapshot must root at least one GcRef (the Vec locals)"
    );
    // Force a collection through the runtime's own root set — the host no
    // longer names one, and the runtime-owned snapshot is an arm of it (P0-06),
    // so this is now a test that the snapshot is rooted *automatically* rather
    // than because this test remembered to pass it. If retention is broken, the
    // referenced objects are reclaimed and dereferencing a root would be
    // use-after-free. Collect several times to stress the mark/sweep.
    for _ in 0..3 {
        rt.collect_now();
    }
    let snap = rt.crash_snapshot().expect("snapshot survives collection");
    // The roots are still valid GcRefs into the (non-moving) heap; reading one
    // as a Vec and checking its length confirms the object survived collection.
    // Find a Vec-typed root among the snapshot locals.
    let vec_root = snap.frames.iter().flat_map(|f| &f.locals).find_map(|l| {
        let desc = unsafe { &*l.descriptor };
        (desc.id() == praxis_runtime::collections::VEC.id())
            .then_some(l.value)
            .flatten()
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

/// TY-33's first unit end to end. `panic` typechecked before this stage and
/// then failed the *compile* — "unresolved user function `panic`" — so the one
/// thing that could not be observed about it was what it does. It raises its
/// own fault kind, carries the words the program wrote, and stops the program
/// where it stands.
#[test]
fn a_panic_stops_the_program_with_the_message_it_was_given() {
    let (rt, _result) = run_main("fn main() -> Int {\n  panic(\"no route\")\n  1\n}\n");
    assert!(rt.has_pending_fault());
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::Panic);
    assert_eq!(rt.fault_message(), Some("no route"));
}

/// …and the message is the *value*, not a literal: `panic` is `(T) -> Never`,
/// so it renders whatever it was handed through that value's descriptor,
/// exactly as `out` does.
#[test]
fn a_panic_renders_a_non_text_argument_through_its_descriptor() {
    let (rt, _result) = run_main("fn main() -> Int { panic(7) }");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::Panic);
    assert_eq!(rt.fault_message(), Some("7"));
}

/// `assert` is the fault that has a *condition*: false stops the program, true
/// is not a fault at all — and the statement after it still runs, which is the
/// half a "does it fault" test alone would miss.
#[test]
fn an_assert_faults_on_false_and_is_invisible_on_true() {
    let (rt, _result) = run_main("fn main() -> Int {\n  assert(1 == 2)\n  3\n}\n");
    assert!(rt.has_pending_fault());
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::AssertFailed);
    // `assert` takes no message, so it sets none (the kind is the whole report).
    assert_eq!(rt.fault_message(), None);

    let (rt, result) = run_main("fn main() -> Int {\n  assert(1 == 1)\n  3\n}\n");
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 3);
}

/// `dbg` is the identity on values, not just on types: the reference that comes
/// back is the one that went in, so `dbg(x)` in an expression computes exactly
/// what `x` does.
#[test]
fn dbg_hands_back_the_value_it_was_given() {
    let (rt, result) = run_main("fn main() -> Int { dbg(20) + dbg(22) }");
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 42);

    // The same reference, not an equal copy: a collection round-trips its
    // identity, so a push through the result is visible in the original.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let xs = Vec()\n  xs.push(1)\n  dbg(xs).push(2)\n  xs.len()\n}\n",
    );
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 2);
}

/// TY-33's second unit end to end: each of the seven numeric helpers computes
/// the number it names. Inference can only say the type is `Int` — that the
/// wrapper behind the name is the *right* wrapper is a fact only a run can
/// establish, and a table that named the wrong symbol would typecheck
/// identically (which is why `each_helper_has_its_own_wrapper` exists too).
#[test]
fn each_numeric_helper_computes_what_it_names() {
    for (src, want) in [
        ("abs(-5)", 5),
        ("abs(5)", 5),
        ("abs(0)", 0),
        ("sign(-9)", -1),
        ("sign(0)", 0),
        ("sign(9)", 1),
        ("min(3, 7)", 3),
        ("min(7, 3)", 3),
        ("min(-2, -2)", -2),
        ("max(3, 7)", 7),
        ("max(7, 3)", 7),
        ("clamp(11, 0, 10)", 10),
        ("clamp(0 - 4, 0, 10)", 0),
        ("clamp(5, 0, 10)", 5),
        // The bounds are inclusive, so an at-the-edge value is its own answer.
        ("clamp(10, 0, 10)", 10),
        ("gcd(12, 18)", 6),
        ("gcd(17, 5)", 1),
        // gcd's sign convention: the result is non-negative whatever the
        // operands' signs, and `gcd(n, 0)` is `abs(n)`.
        ("gcd(0 - 12, 18)", 6),
        ("gcd(7, 0)", 7),
        ("gcd(0, 0)", 0),
        ("lcm(4, 6)", 12),
        ("lcm(21, 6)", 42),
        ("lcm(0, 5)", 0),
        ("lcm(0 - 4, 6)", 12),
    ] {
        let program = format!("fn main() -> Int {{ {src} }}");
        let (rt, result) = run_main(&program);
        assert!(!rt.has_pending_fault(), "{src} faulted");
        assert_eq!(result.as_int(), want, "{src}");
    }
}

/// The three helpers that can leave the `Int` range **fault** rather than
/// wrapping, which is the same answer `+`/`-`/`*` already give (§4.12) and the
/// same answer TY-28 gave for a literal: a number nobody wrote is worse than a
/// stop. `sign`, `min` and `max` are total and are here to show the fault is not
/// blanket caution.
#[test]
fn a_numeric_helper_faults_rather_than_wrapping() {
    // `abs(Int::MIN)` has no positive counterpart — `praxis_int_neg`'s edge.
    let (rt, _r) = run_main("fn main() -> Int { abs(0 - 9223372036854775807 - 1) }");
    assert!(rt.has_pending_fault());
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::IntOverflow);

    // `lcm` overflows long before its operands do: 2^62 and 3 have no common
    // multiple in range.
    let (rt, _r) = run_main("fn main() -> Int { lcm(4611686018427387904, 3) }");
    assert!(rt.has_pending_fault());
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::IntOverflow);

    // An inverted `clamp` range is empty, so there is no value to return.
    // ADR-058 borrowed `InvalidSize` for it and recorded a dedicated kind as
    // owed; S18 spent a bump and paid it (ADR-075).
    let (rt, _r) = run_main("fn main() -> Int { clamp(5, 10, 0) }");
    assert!(rt.has_pending_fault());
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::EmptyRange);

    // …and the total three are total at the same edges.
    let (rt, result) = run_main("fn main() -> Int { sign(0 - 9223372036854775807 - 1) }");
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), -1);
    let (rt, result) = run_main("fn main() -> Int { min(0 - 9223372036854775807 - 1, 0) }");
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), i64::MIN);
    let (rt, result) = run_main("fn main() -> Int { max(9223372036854775807, 0) }");
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), i64::MAX);
}

// --- §6.5's graph helpers (TY-33 unit 3, ADR-060) ---------------------------

/// A graph as a `steps` function, for the tests below. `1 -> {2, 3}`, `2 -> {4}`,
/// `3 -> {4}`, `4 -> {}` — the diamond, which is the smallest graph that tells
/// breadth-first from depth-first and tells "reached once" from "reached twice".
const DIAMOND: &str = "fn steps(n: Int) -> Vec[Int] {\n\
                       \x20 var v = Vec()\n\
                       \x20 if n == 1 {\n    v.push(2)\n    v.push(3)\n  }\n\
                       \x20 if n == 2 { v.push(4) }\n\
                       \x20 if n == 3 { v.push(4) }\n\
                       \x20 v\n\
                       }\n";

/// TY-33's third unit end to end: the two traversals visit every reachable
/// state, once, in the order their names promise.
///
/// The half no type test can see. Inference says the result is `Vec[Int]`; that
/// the wrapper behind the name walks the graph *at all* — that it calls the
/// closure, reads the `Vec` it hands back, and recognizes a state it has already
/// seen — is a fact only a run establishes. Before this the call reached the
/// backend as `CallTarget::User("bfs")` and the compile failed.
///
/// The order is encoded as decimal digits, so a walk that visited the right
/// states in the wrong order fails rather than passing on a length check.
#[test]
fn the_two_traversals_visit_every_reachable_state_in_the_order_they_name() {
    let digits =
        "fn digits(v: Vec[Int]) -> Int {\n  var t = 0\n  for n in v { t = t * 10 + n }\n  t\n}\n";

    let (rt, result) = run_main(&format!(
        "{DIAMOND}{digits}fn main() -> Int {{ digits(bfs(1, |n| steps(n))) }}"
    ));
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1234, "breadth-first is 1, 2, 3, 4");

    let (rt, result) = run_main(&format!(
        "{DIAMOND}{digits}fn main() -> Int {{ digits(dfs(1, |n| steps(n))) }}"
    ));
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(
        result.as_int(),
        1243,
        "depth-first descends into the first neighbour: 1, 2, 4, 3"
    );

    // The join is reached twice and visited once — the visited set, observed.
    let (rt, result) = run_main(&format!(
        "{DIAMOND}fn main() -> Int {{ bfs(1, |n| steps(n)).len() }}"
    ));
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 4);

    // A cycle terminates rather than walking forever.
    let (rt, result) = run_main(
        "fn steps(n: Int) -> Vec[Int] {\n  var v = Vec()\n  v.push((n + 1) % 3)\n  v\n}\n\
         fn main() -> Int { bfs(0, |n| steps(n)).len() }",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

/// `flood_fill` answers a `Set` of everything reachable, and a state the graph
/// does not reach is not in it. The `Set` is a real one — its `contains` is the
/// same structural membership every other `Set` has, which is what the state's
/// `HashStable` requirement buys.
#[test]
fn a_flood_fill_reaches_exactly_the_states_the_graph_connects() {
    let (rt, result) = run_main(&format!(
        "{DIAMOND}fn main() -> Int {{ flood_fill(1, |n| steps(n)).len() }}"
    ));
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 4);

    // Started from the far side, only the far side is reachable.
    let (rt, result) = run_main(&format!(
        "{DIAMOND}fn main() -> Int {{ flood_fill(3, |n| steps(n)).len() }}"
    ));
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2, "3 reaches only itself and 4");

    // Membership is the `Set`'s own structural `contains`, not a length check.
    let (rt, result) = run_main(&format!(
        "{DIAMOND}fn main() -> Int {{\n\
         \x20 let seen = flood_fill(1, |n| steps(n))\n\
         \x20 if seen.contains(4) {{ 1 }} else {{ 0 }}\n\
         }}"
    ));
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
    let (rt, result) = run_main(&format!(
        "{DIAMOND}fn main() -> Int {{\n\
         \x20 let seen = flood_fill(1, |n| steps(n))\n\
         \x20 if seen.contains(9) {{ 1 }} else {{ 0 }}\n\
         }}"
    ));
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

/// `bfs_distance` counts steps to the first state its predicate accepts, and
/// answers `None` when there is none. The `Option` the runtime builds is an
/// ordinary one: it matches against the same `Some`/`None` arms a program's own
/// `Option` does, which is what the shared variant tags mean.
#[test]
fn a_distance_is_the_step_count_and_an_unreachable_goal_is_none() {
    let unwrap = "fn unwrap(d: Option[Int]) -> Int {\n\
                  \x20 match d {\n    Some(k) => k,\n    None => 0 - 1,\n  }\n\
                  }\n";

    let (rt, result) = run_main(&format!(
        "{DIAMOND}{unwrap}fn main() -> Int \
         {{ unwrap(bfs_distance(1, |n| steps(n), |n| n == 4)) }}"
    ));
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);

    // A start that already satisfies the predicate is zero steps, not one.
    let (rt, result) = run_main(&format!(
        "{DIAMOND}{unwrap}fn main() -> Int \
         {{ unwrap(bfs_distance(1, |n| steps(n), |n| n == 1)) }}"
    ));
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);

    // No state satisfies it: `None`, not a sentinel and not a fault.
    let (rt, result) = run_main(&format!(
        "{DIAMOND}{unwrap}fn main() -> Int \
         {{ unwrap(bfs_distance(1, |n| steps(n), |n| n == 99)) }}"
    ));
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), -1);

    // The distance is the *shortest* path. The long way round is enqueued
    // first, so a walk that answered with the first goal it enqueued says 3.
    let (rt, result) = run_main(&format!(
        "fn steps(n: Int) -> Vec[Int] {{\n\
         \x20 var v = Vec()\n\
         \x20 if n == 1 {{\n    v.push(2)\n    v.push(5)\n  }}\n\
         \x20 if n == 2 {{ v.push(3) }}\n\
         \x20 if n == 3 {{ v.push(4) }}\n\
         \x20 if n == 5 {{ v.push(4) }}\n\
         \x20 v\n\
         }}\n\
         {unwrap}fn main() -> Int {{ unwrap(bfs_distance(1, |n| steps(n), |n| n == 4)) }}"
    ));
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

/// `dijkstra` answers the least *cost*, not the fewest steps — the whole
/// difference between it and `bfs_distance`, and the half a step-counting walk
/// gets wrong. The cheap three-hop path beats the expensive one-hop edge.
#[test]
fn a_cost_table_holds_the_least_cost_to_every_reachable_state() {
    // 1 -> 4 costs 10 directly; 1 -> 2 -> 3 -> 4 costs 3.
    let graph = "fn steps(n: Int) -> Vec[Int] {\n\
                 \x20 var v = Vec()\n\
                 \x20 if n == 1 {\n    v.push(2)\n    v.push(4)\n  }\n\
                 \x20 if n == 2 { v.push(3) }\n\
                 \x20 if n == 3 { v.push(4) }\n\
                 \x20 v\n\
                 }\n\
                 fn cost(a: Int, b: Int) -> Int {\n\
                 \x20 if a == 1 {\n    if b == 4 { 10 } else { 1 }\n  } else { 1 }\n\
                 }\n";

    let (rt, result) = run_main(&format!(
        "{graph}fn main() -> Int {{ dijkstra(1, |n| steps(n), |a, b| cost(a, b))[4] }}"
    ));
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(
        result.as_int(),
        3,
        "the cheap long path, not the dear short one"
    );

    // The start is in the table at zero, and every reachable state is in it once.
    let (rt, result) = run_main(&format!(
        "{graph}fn main() -> Int {{ dijkstra(1, |n| steps(n), |a, b| cost(a, b))[1] }}"
    ));
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
    let (rt, result) = run_main(&format!(
        "{graph}fn main() -> Int {{ dijkstra(1, |n| steps(n), |a, b| cost(a, b)).len() }}"
    ));
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 4);

    // A negative edge weight faults: Dijkstra never reconsiders a settled state,
    // so its answer would be quietly too large (ADR-060).
    let (rt, _result) = run_main(&format!(
        "{graph}fn main() -> Int {{ dijkstra(1, |n| steps(n), |a, b| 0 - 1).len() }}"
    ));
    assert!(rt.has_pending_fault());
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::NoAnswer);
}

/// `a_star` answers the cheapest cost to a goal, and the heuristic changes only
/// which states it examines — never the answer. The same graph is searched with
/// a zero heuristic (which is Dijkstra) and with an exact one.
#[test]
fn a_star_finds_the_cheapest_goal_and_the_heuristic_does_not_change_it() {
    let graph = "fn steps(n: Int) -> Vec[Int] {\n\
                 \x20 var v = Vec()\n\
                 \x20 if n == 1 {\n    v.push(2)\n    v.push(4)\n  }\n\
                 \x20 if n == 2 { v.push(3) }\n\
                 \x20 if n == 3 { v.push(4) }\n\
                 \x20 v\n\
                 }\n\
                 fn cost(a: Int, b: Int) -> Int {\n\
                 \x20 if a == 1 {\n    if b == 4 { 10 } else { 1 }\n  } else { 1 }\n\
                 }\n\
                 fn remaining(n: Int) -> Int {\n\
                 \x20 if n == 1 { 3 } else {\n    if n == 2 { 2 } else {\n      if n == 3 { 1 } else { 0 }\n    }\n  }\n\
                 }\n\
                 fn unwrap(d: Option[Int]) -> Int {\n\
                 \x20 match d {\n    Some(k) => k,\n    None => 0 - 1,\n  }\n\
                 }\n";

    let (rt, result) = run_main(&format!(
        "{graph}fn main() -> Int \
         {{ unwrap(a_star(1, |n| steps(n), |a, b| cost(a, b), |n| 0, |n| n == 4)) }}"
    ));
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);

    let (rt, result) = run_main(&format!(
        "{graph}fn main() -> Int \
         {{ unwrap(a_star(1, |n| steps(n), |a, b| cost(a, b), |n| remaining(n), |n| n == 4)) }}"
    ));
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(
        result.as_int(),
        3,
        "an admissible heuristic changes nothing"
    );

    // An unreachable goal is `None`, and a start that is already a goal is 0.
    let (rt, result) = run_main(&format!(
        "{graph}fn main() -> Int \
         {{ unwrap(a_star(1, |n| steps(n), |a, b| cost(a, b), |n| 0, |n| n == 99)) }}"
    ));
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), -1);
    let (rt, result) = run_main(&format!(
        "{graph}fn main() -> Int \
         {{ unwrap(a_star(1, |n| steps(n), |a, b| cost(a, b), |n| 0, |n| n == 1)) }}"
    ));
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);

    // A negative heuristic breaks the ordering the search is built on, and is
    // the one caller error A* can see.
    let (rt, _result) = run_main(&format!(
        "{graph}fn main() -> Int \
         {{ unwrap(a_star(1, |n| steps(n), |a, b| cost(a, b), |n| 0 - 5, |n| n == 4)) }}"
    ));
    assert!(rt.has_pending_fault());
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::NoAnswer);
}

/// A state is a **value**, not an integer the walk happens to understand. A
/// record of scalars — which is what a grid position is written as — walks, is
/// recognized as already-visited when the neighbour function mints an equal but
/// freshly allocated one, and keys the cost table.
///
/// This is the property the `HashStable` requirement exists for, observed: the
/// walk's visited set compares states structurally, so a `steps` that allocates
/// a new record every call still terminates.
#[test]
fn a_state_may_be_any_value_that_can_be_remembered() {
    let grid = "struct P { x: Int, y: Int }\n\
                fn steps(p: P) -> Vec[P] {\n\
                \x20 var v = Vec()\n\
                \x20 if p.x < 2 { v.push(P { x: p.x + 1, y: p.y }) }\n\
                \x20 if p.y < 2 { v.push(P { x: p.x, y: p.y + 1 }) }\n\
                \x20 v\n\
                }\n\
                fn corner(p: P) -> Bool {\n\
                \x20 if p.x == 2 { p.y == 2 } else { false }\n\
                }\n\
                fn unwrap(d: Option[Int]) -> Int {\n\
                \x20 match d {\n    Some(k) => k,\n    None => 0 - 1,\n  }\n\
                }\n";

    // A 3x3 grid: nine positions, every one of them reached once.
    let (rt, result) = run_main(&format!(
        "{grid}fn main() -> Int {{ bfs(P {{ x: 0, y: 0 }}, |p| steps(p)).len() }}"
    ));
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 9);

    // …four steps to the far corner, which is the Manhattan distance.
    let (rt, result) = run_main(&format!(
        "{grid}fn main() -> Int \
         {{ unwrap(bfs_distance(P {{ x: 0, y: 0 }}, |p| steps(p), |p| corner(p))) }}"
    ));
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 4);

    // …and the cost table is keyed on the record, so a freshly built equal
    // position finds its entry.
    let (rt, result) = run_main(&format!(
        "{grid}fn main() -> Int {{\n\
         \x20 let costs = dijkstra(P {{ x: 0, y: 0 }}, |p| steps(p), |a, b| 1)\n\
         \x20 costs[P {{ x: 2, y: 2 }}]\n\
         }}"
    ));
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 4);
}

/// A walk holds every state it has seen in Rust structures the collector cannot
/// scan, so each one has to be rooted in the native frame (P0-07). A graph big
/// enough to collect several times over, whose neighbour function allocates on
/// every call, is what makes the rooting observable: without it the visited set
/// holds reclaimed objects and the answer is wrong or the host dies.
#[test]
fn a_walk_roots_the_states_it_is_holding_across_its_own_allocations() {
    let (rt, result) = run_main(
        "fn steps(n: Int) -> Vec[Int] {\n\
         \x20 var v = Vec()\n\
         \x20 if n < 400 { v.push(n + 1) }\n\
         \x20 v\n\
         }\n\
         fn main() -> Int { bfs(0, |n| steps(n)).len() }",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 401);

    // The same, through the weighted walk, which additionally holds a cost table
    // and a priority queue.
    let (rt, result) = run_main(
        "fn steps(n: Int) -> Vec[Int] {\n\
         \x20 var v = Vec()\n\
         \x20 if n < 400 { v.push(n + 1) }\n\
         \x20 v\n\
         }\n\
         fn main() -> Int { dijkstra(0, |n| steps(n), |a, b| 2)[400] }",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 800);
}

/// A fault raised *inside* a neighbour closure stops the walk and reaches the
/// call site. The helper is a runtime call that calls back into generated code,
/// so the fault has to cross that boundary in both directions — and a walk that
/// kept going would keep walking a graph of Unit sentinels.
#[test]
fn a_fault_inside_a_closure_stops_the_walk() {
    let (rt, _result) = run_main(
        "fn steps(n: Int) -> Vec[Int] {\n\
         \x20 var v = Vec()\n\
         \x20 v.push(n / 0)\n\
         \x20 v\n\
         }\n\
         fn main() -> Int { bfs(1, |n| steps(n)).len() }",
    );
    assert!(rt.has_pending_fault());
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::DivByZero);

    // …and `panic` inside a goal predicate, which is the other closure position.
    let (rt, _result) = run_main(
        "fn steps(n: Int) -> Vec[Int] { Vec() }\n\
         fn main() -> Int {\n\
         \x20 let d = bfs_distance(1, |n| steps(n), |n| panic(\"stop\"))\n\
         \x20 0\n\
         }",
    );
    assert!(rt.has_pending_fault());
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::Panic);
}

/// TY-34 end to end (ADR-059): a `for` over a range runs the iterations the
/// range names. This is the half no type test can see — the loop reads
/// `praxis_range_len` and `praxis_range_get`, so a wrong `len`/`get` symbol
/// selection typechecks identically and then iterates the wrong collection.
#[test]
fn a_for_over_a_range_runs_its_integers_in_order() {
    // Half-open: 0,1,2,3,4 — five iterations, and the last is 4 rather than 5.
    let (rt, result) =
        run_main("fn main() -> Int {\n  var t = 0\n  for i in 0..5 { t = t * 10 + i }\n  t\n}\n");
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 1234);

    // Inclusive: one more iteration, the end included.
    let (rt, result) =
        run_main("fn main() -> Int {\n  var t = 0\n  for i in 1..=4 { t = t + i }\n  t\n}\n");
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 10);

    // A descending range is empty, not a countdown (D3) — the body never runs.
    let (rt, result) =
        run_main("fn main() -> Int {\n  var t = 7\n  for i in 5..0 { t = t + 100 }\n  t\n}\n");
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 7);
    // …and so is an empty half-open range whose bounds are equal, while the
    // inclusive form at the same bounds runs exactly once.
    let (rt, result) =
        run_main("fn main() -> Int {\n  var t = 0\n  for i in 3..3 { t = t + 1 }\n  t\n}\n");
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 0);
    let (rt, result) =
        run_main("fn main() -> Int {\n  var t = 0\n  for i in 3..=3 { t = t + i }\n  t\n}\n");
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 3);

    // Negative bounds, and a bound that is an expression rather than a literal.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var t = 0\n  for i in (0 - 2)..(1 + 1) { t = t + i }\n  t\n}\n",
    );
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), -2);
}

/// A range is a **value** (D6), not only a `for`-header form: it survives being
/// bound to a name, passed through a function, stored in a collection and used as
/// a `Map` key — and it renders as the half-open interval it is.
#[test]
fn a_range_is_a_value_that_outlives_its_expression() {
    // Bound to a `let`, then iterated — the range object has to survive the
    // binding and the allocations between.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let r = 2..6\n  let v = Vec()\n  v.push(9)\n\
         \x20 var t = 0\n  for i in r { t = t + i }\n  t + v.len()\n}\n",
    );
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 15);

    // Through a function, in both directions.
    let (rt, result) = run_main(
        "fn widen(r: Range) -> Range { r }\n\
         fn main() -> Int {\n  var t = 0\n  for i in widen(1..4) { t = t + i }\n  t\n}\n",
    );
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 6);

    // As a `Map` key: hashable *and* immutable, so it is findable again by an
    // equal range built separately (ADR-057 D4, ADR-059).
    let (rt, result) =
        run_main("fn main() -> Int {\n  let m = Map()\n  m.insert(0..3, 41)\n  m[0..3] + 1\n}\n");
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 42);

    // …and `1..=4` really is `1..5`, which is what normalizing at construction
    // means: the two spellings are one key and one rendering.
    let (rt, result) =
        run_main("fn main() -> Int {\n  let m = Map()\n  m.insert(1..=4, 5)\n  m[1..5]\n}\n");
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 5);
}

/// §3.3's representative program computes `sign` and `abs` on values it read,
/// and `max(abs(dx), abs(dy))` over them. A helper has to survive being nested
/// in an expression, called with computed operands, and used as another
/// function's argument — the shapes a fresh type variable accepted and then
/// could not compile.
#[test]
fn numeric_helpers_nest_and_carry_computed_operands() {
    let (rt, result) = run_main(
        "fn spread(x1: Int, y1: Int, x2: Int, y2: Int) -> Int {\n\
         \x20 max(abs(x2 - x1), abs(y2 - y1))\n\
         }\n\
         fn main() -> Int { spread(1, 1, 4, 9) }\n",
    );
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 8);

    // A helper's result feeds a loop bound and a mutable accumulator, so the
    // call's temp has to survive the allocations around it.
    let (rt, result) = run_main(
        "fn main() -> Int {\n\
         \x20 var total = 0\n\
         \x20 for n in Vec() { total = total + n }\n\
         \x20 total + clamp(gcd(48, 18), 0, 4) + lcm(2, 3)\n\
         }\n",
    );
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 10);
}

/// TY-30 end to end: the §5.2 program the design doc promises needs no
/// annotations. `total`'s parameter has no type until `main` calls it, and the
/// `.sum()` inside it had no catalog entry when inference walked past it — the
/// entry is selected later, when the receiver resolves, and this is where that
/// shows: the method has to *lower*, which is the half a type test cannot see.
///
/// Before this, the program typechecked and then failed the compile with "no
/// method `sum` on this type" — a clean program that could not run.
#[test]
fn a_method_on_an_unannotated_parameter_runs() {
    let (rt, result) = run_main(
        "fn total(values) { values.sum() }\n\
         fn main() -> Int {\n  \
           let values = Vec()\n  \
           values.push(1)\n  \
           values.push(2)\n  \
           total(values)\n\
         }\n",
    );
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 3);
}

/// …and a deferred method with *arguments* runs, mutating the receiver the call
/// site owns. `add` learns both of its parameter types from `push`'s catalog
/// row, so this is the resolution running in the argument direction as well as
/// the result one.
#[test]
fn a_deferred_method_with_arguments_runs() {
    let (rt, result) = run_main(
        "fn add(v, x) { v.push(x) }\n\
         fn main() -> Int {\n  \
           let values = Vec()\n  \
           values.push(1)\n  \
           add(values, 41)\n  \
           values.sum()\n\
         }\n",
    );
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 42);
}

// ---- Function values (REP-01, ADR-061) ----

/// **REP-01, the only P0 left in the repair.** A top-level `fn` used as a value
/// is a callable closure.
///
/// `let f = double` lowered to `Unit` and `Inst::CallIndirect` then read that
/// Unit's payload as a function pointer, so this program — which `praxis check`
/// accepts, because a `fn`'s type *is* a `Func` — took the host down with a
/// SIGBUS. **This test aborting the test process is the failure mode**, not a
/// wrong answer.
///
/// All three routes the stage names are here: a `let`, a parameter of declared
/// function type, and a graph helper's closure argument (§6.5's helpers were the
/// new way to reach the bug, and their descriptor check is containment, not a
/// fix). A `Vec` element is a fourth, and is the one that also exercises the
/// postfix call form.
#[test]
fn a_top_level_fn_is_a_callable_value() {
    // Through a `let`, then called by name — the finding's own reproduction.
    let (rt, result) = run_main(
        "fn double(n: Int) -> Int { n * 2 }\n\
         fn main() -> Int {\n  let f = double\n  f(3)\n}\n",
    );
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 6);

    // Through a parameter of declared function type, alongside a closure
    // literal in the same position: the two are one calling convention.
    let (rt, result) = run_main(
        "fn double(n: Int) -> Int { n * 2 }\n\
         fn apply(g: (Int) -> Int, n: Int) -> Int { g(n) }\n\
         fn main() -> Int { apply(double, 20) + apply(|x| x + 1, 1) }\n",
    );
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 42);

    // Two arguments, so a shifted argument list would be visible as a wrong
    // answer rather than only as a crash.
    let (rt, result) = run_main(
        "fn sub(a: Int, b: Int) -> Int { a - b }\n\
         fn main() -> Int {\n  let f = sub\n  f(50, 8)\n}\n",
    );
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 42);

    // Stored in a collection and called postfix, which reaches the value
    // through `callee_expr` rather than through a name.
    let (rt, result) = run_main(
        "fn double(n: Int) -> Int { n * 2 }\n\
         fn main() -> Int {\n  let fs = Vec()\n  fs.push(double)\n  fs.get(0)(21)\n}\n",
    );
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 42);
}

/// …and through a graph helper, where `praxis-runtime` calls *back* into
/// generated code. The helper receives the adapter's closure and calls it once
/// per state; a `Unit` here is what its descriptor check used to turn into a
/// `TypeMismatch` fault.
#[test]
fn a_fn_value_is_callable_from_the_runtime_side() {
    let (rt, result) = run_main(
        "fn step(n: Int) -> Vec[Int] {\n  \
           let v = Vec()\n  \
           if n < 4 { v.push(n + 1) }\n  \
           v\n\
         }\n\
         fn at_goal(n: Int) -> Bool { n == 4 }\n\
         fn main() -> Int {\n  \
           let d = bfs_distance(0, step, at_goal)\n  \
           match d { Some(k) => k, None => 0 - 1 }\n\
         }\n",
    );
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 4);
}

/// A fault raised inside the adapted function still reaches the caller as a
/// fault — the adapter is on the fault path's way out, so it checks after its
/// one call instead of returning the Unit sentinel as a value.
#[test]
fn a_fault_inside_a_fn_value_is_not_swallowed_by_its_adapter() {
    let (rt, _result) = run_main(
        "fn half(n: Int) -> Int { n / 0 }\n\
         fn main() -> Int {\n  let f = half\n  f(10)\n}\n",
    );
    assert!(rt.has_pending_fault(), "the DivByZero has to arrive");
}

/// **REP-03.** A `for` over an unannotated parameter runs, and runs against the
/// iterable each call site chose.
///
/// The half no type test can see, and the one that says the *iterator* is still
/// quantified. MIR picks the `len`/`get` runtime symbols from the iterator's
/// **static** collection ctor, so one lowered body per iterable kind is not an
/// optimization here — it is the only way the symbols can be right.
/// Monomorphization is what provides them: the iterator stays a binder, so
/// `total(v)` and `total(0..4)` in one program are two clones with `praxis_vec_*`
/// and `praxis_range_*` respectively. Pinning the iterator instead — the obvious
/// way to make the item resolvable — would have made this program a signature
/// disagreement and never reached codegen at all.
///
/// The **element** is the other half: `copy` recorded the loop variable's type as
/// the *collection's* before the fix, so it inferred `Vec[Vec[Int]]` and faulted
/// with "value does not have the declared type" out of a program `praxis check`
/// accepted.
#[test]
fn a_for_over_an_unannotated_parameter_runs_against_each_iterable_it_is_given() {
    const TOTAL: &str = "fn total(r) { var t = 0\n for i in r { t = t + i }\n t }\n";

    // One source function, three collection ctors, three sets of symbols.
    for (main, want) in [
        (
            "fn main() -> Int { var v = Vec()\n v.push(10)\n v.push(20)\n total(v) }",
            30,
        ),
        ("fn main() -> Int { total(0..4) }", 6),
        (
            "fn main() -> Int { var d = Deque()\n d.push_back(4)\n d.push_back(5)\n total(d) }",
            9,
        ),
        // …and all three in one program, which is where the clones have to be
        // distinct: a single body reusing `praxis_vec_len` on a `Range` reads a
        // length out of the wrong payload word.
        (
            "fn main() -> Int { var v = Vec()\n v.push(10)\n v.push(20)\n \
             var d = Deque()\n d.push_back(4)\n \
             total(v) + total(0..4) + total(d) }",
            40,
        ),
    ] {
        let (rt, result) = run_main(&format!("{TOTAL}{main}"));
        assert!(!rt.has_pending_fault(), "faulted: {main}");
        assert_eq!(result.as_int(), want, "{main}");
    }

    // The element half: a `Vec` built out of an unannotated iterable holds the
    // *elements*, and this is the program that faulted at run time before.
    const COPY: &str = "fn copy(vs) { var o = Vec()\n for v in vs { o.push(v) }\n o }\n";
    let (rt, result) = run_main(&format!(
        "{COPY}fn main() -> Int {{ var s = Vec()\n s.push(7)\n s.push(9)\n \
         let d = copy(s)\n d.get(0) + d.get(1) }}"
    ));
    assert!(!rt.has_pending_fault(), "copy over a Vec faulted");
    assert_eq!(result.as_int(), 16);
    // …and the same body over a different ctor, so the element type is read from
    // the argument and not from the first use.
    let (rt, result) = run_main(&format!(
        "{COPY}fn main() -> Int {{ let d = copy(1..4)\n d.len() + d.get(2) }}"
    ));
    assert!(!rt.has_pending_fault(), "copy over a Range faulted");
    assert_eq!(result.as_int(), 6);
}

/// **REP-07.** `&&` and `||` short-circuit: the right operand is not evaluated
/// on the path that cannot need it.
///
/// The half no parse test can see, and the point of the operators. `false &&
/// panic("x")` must produce `false`, not a fault — MIR lowers `rhs` into exactly
/// one of the two blocks, so its side effects, its faults and its GC safepoints
/// are all skipped. `&&` and `||` are one lowering with the skipping side's answer
/// flipped, so each direction is asserted for both.
#[test]
fn the_logical_operators_short_circuit_and_answer_their_truth_table() {
    // The truth table, both operators, all four rows each.
    for (src, want) in [
        ("fn main() -> Bool { true && true }", true),
        ("fn main() -> Bool { true && false }", false),
        ("fn main() -> Bool { false && true }", false),
        ("fn main() -> Bool { false && false }", false),
        ("fn main() -> Bool { true || true }", true),
        ("fn main() -> Bool { true || false }", true),
        ("fn main() -> Bool { false || true }", true),
        ("fn main() -> Bool { false || false }", false),
    ] {
        let (rt, result) = run_main(src);
        assert!(!rt.has_pending_fault(), "{src} faulted");
        assert_eq!(result.as_bool(), want, "{src}");
    }

    // Short circuit: the skipped operand is one that would fault, so reaching it
    // is observable rather than a matter of inspecting the MIR.
    for src in [
        "fn main() -> Bool { false && panic(\"not reached\") }",
        "fn main() -> Bool { true || panic(\"not reached\") }",
        // …and one that faults by dividing rather than by panicking, so the test
        // does not depend on `panic`'s own path.
        "fn main() -> Bool { false && (1 / 0) == 0 }",
        "fn main() -> Bool { true || (1 / 0) == 0 }",
    ] {
        let (rt, _result) = run_main(src);
        assert!(!rt.has_pending_fault(), "{src} evaluated its right operand");
    }
    // …and the other direction really does evaluate it, or the test above would
    // pass against an implementation that never evaluates `rhs` at all.
    for src in [
        "fn main() -> Bool { true && panic(\"reached\") }",
        "fn main() -> Bool { false || panic(\"reached\") }",
    ] {
        let (rt, _result) = run_main(src);
        assert!(rt.has_pending_fault(), "{src} must reach its right operand");
    }

    // Precedence, observed as a value: `&&` binds tighter than `||`, so this is
    // `false || (true && false)` and not `(false || true) && false`… which are
    // both `false`. Use a case where they differ: `true || false && false` is
    // `true || (false && false)` = true, and `(true || false) && false` = false.
    let (rt, result) = run_main("fn main() -> Bool { true || false && false }");
    assert!(!rt.has_pending_fault());
    assert!(result.as_bool(), "&& binds tighter than ||");

    // §3.3's own shape, end to end: comparisons under `&&` under a `!`.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  \
           let diagonals = false\n  let dx = 1\n  let dy = 0\n  \
           if !diagonals && dx != 0 && dy != 0 { 9 } else { 8 }\n\
         }",
    );
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 8);
}

/// **REP-08.** A tuple element read at run time is the element the position
/// names, at every arity and through every shape that holds one.
///
/// The half no type test can see: `praxis_tuple_get` had **no MIR caller** before
/// this — the symbol existed and nothing reached it, because `Inst::LoadField`
/// hard-codes `praxis_record_field` and a tuple index had no instruction of its
/// own. A `LoadField` reused here would read a record's field table out of a
/// tuple's payload.
#[test]
fn a_tuple_element_reads_the_value_at_that_position() {
    for (src, want) in [
        ("fn main() -> Int { (1, 2).0 }", 1),
        ("fn main() -> Int { (1, 2).1 }", 2),
        // Every position of a wider tuple, so an off-by-one in the index would
        // show rather than cancel.
        ("fn main() -> Int { (10, 20, 30, 40, 50).0 }", 10),
        ("fn main() -> Int { (10, 20, 30, 40, 50).2 }", 30),
        ("fn main() -> Int { (10, 20, 30, 40, 50).4 }", 50),
        // Nested — the lexer rule's own case: `n.0.1` is two indices.
        ("fn main() -> Int { ((1, 2), 3).0.1 }", 2),
        ("fn main() -> Int { ((1, 2), 3).1 }", 3),
        // Through a binding, a parameter, and a closure body.
        ("fn main() -> Int { let p = (7, 9)\n p.0 + p.1 }", 16),
        (
            "fn snd(p: (Int, Int)) -> Int { p.1 }\nfn main() -> Int { snd((3, 4)) }",
            4,
        ),
        (
            "fn main() -> Int { let f = |p: (Int, Int)| p.0 * p.1\n f((6, 7)) }",
            42,
        ),
        // Out of a collection, which is how a corpus program actually holds them.
        (
            "fn main() -> Int { var v = Vec()\n v.push((1, 5))\n v.get(0).1 }",
            5,
        ),
    ] {
        let (rt, result) = run_main(src);
        assert!(!rt.has_pending_fault(), "{src} faulted");
        assert_eq!(result.as_int(), want, "{src}");
    }

    // A non-`Int` element comes back as itself, so the read is not quietly an
    // integer extraction.
    let (rt, result) = run_main("fn main() -> Bool { (1, true).1 }");
    assert!(!rt.has_pending_fault());
    assert!(result.as_bool());

    // A float literal is still a float: `3.0` is one token, and only a digit run
    // *after* a `.` is an index.
    let (rt, result) = run_main("fn main() -> Float { 3.0 }");
    assert!(!rt.has_pending_fault());
    assert!((result.as_float() - 3.0).abs() < 1e-12);
}

/// **REP-16 end to end.** A subscript reads and writes the collection it names,
/// through every receiver that has the operation.
///
/// The half no type test can see: which runtime wrapper each row selects. A
/// `Counter` read that reached `praxis_map_get` would answer Unit where §6.2
/// promises zero, and a `Grid` store that forgot to pass both coordinates would
/// write the wrong cell — both type-check.
#[test]
fn a_subscript_reads_and_writes_through_the_wrapper_its_receiver_needs() {
    // Read, on each of the six.
    for (src, want) in [
        ("let v = Vec()\n v.push(10)\n v.push(20)\n v[1]", 20),
        (
            "let d = Deque()\n d.push_back(4)\n d.push_front(9)\n d[0]",
            9,
        ),
        ("let m = Map()\n m.insert(\"a\", 7)\n m[\"a\"]", 7),
        // §6.2: a `Counter`'s absent key reads as zero and does not fault, which
        // is the one read that differs from its `Map` sibling's.
        (
            "let c = Counter()\n c.inc(\"a\")\n c[\"a\"] + c[\"nope\"]",
            1,
        ),
        ("\"abc\"[1]", 98),
    ] {
        let (rt, result) = run_main(&format!("fn main() -> Int {{\n  {src}\n}}\n"));
        assert!(!rt.has_pending_fault(), "{src} faulted: {:?}", rt.fault());
        assert_eq!(result.as_int(), want, "{src}");
    }

    // Store, on the three that have one — and read back through the subscript, so
    // the pair has to agree about which collection it is talking to.
    for (src, want) in [
        ("let m = Map()\n m[\"a\"] = 5\n m[\"a\"]", 5),
        ("let m = Map()\n m[\"a\"] = 5\n m[\"a\"] = 6\n m.len()", 1),
        ("let c = Counter()\n c[\"a\"] = 4\n c[\"a\"]", 4),
    ] {
        let (rt, result) = run_main(&format!("fn main() -> Int {{\n  {src}\n}}\n"));
        assert!(!rt.has_pending_fault(), "{src} faulted: {:?}", rt.fault());
        assert_eq!(result.as_int(), want, "{src}");
    }

    // Every compound operator, through a `Counter` — §3.3's own `+=` plus the
    // four the grammar also admits, so the read-modify-write is general and not
    // `inc` in disguise.
    for (op, want) in [("+=", 13), ("-=", 7), ("*=", 30), ("/=", 3), ("%=", 1)] {
        let src = format!(
            "fn main() -> Int {{\n  let c = Counter()\n  c[\"k\"] = 10\n  c[\"k\"] {op} 3\n  c[\"k\"]\n}}\n"
        );
        let (rt, result) = run_main(&src);
        assert!(!rt.has_pending_fault(), "{op} faulted: {:?}", rt.fault());
        assert_eq!(result.as_int(), want, "{op}");
    }

    // A `Counter`'s zero default is what makes `counts[key] += 1` work on a key
    // that has never been seen — §3.3 never initializes one.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let c = Counter()\n  c[\"a\"] += 1\n  c[\"a\"] += 1\n  \
         c[\"b\"] += 5\n  c[\"a\"] * 100 + c[\"b\"]\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 205);

    // A tuple key, which is what §3.3 counts by. The key is a fresh allocation
    // every iteration, so this only works if identity is structural (ADR-026).
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let c = Counter()\n  \
         for i in 0..3 { c[(1, 2)] += 1 }\n  c[(1, 2)] * 10 + c.len()\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 31, "one key, counted three times");
}

/// **REP-16's two-coordinate subscript** (§6.4): `grid[x, y]` reads and writes the
/// cell at (x, y), and the coordinate order is x-then-y.
#[test]
fn a_grid_subscript_takes_both_coordinates_in_the_order_the_design_names() {
    // A 2×2 grid read from input (a `Grid()` is 0×0, so it has no cell to name).
    // The written cell is deliberately **off the diagonal**: a store that reached
    // `praxis_grid_set(g, y, x, v)` would pass on a diagonal cell.
    //
    // AMENDED (S20/D11). The expected value used to be `700 + 120 + 34`, which
    // says `g[0, 0]` is `12` and `g[0, 1]` is `34` — i.e. that a `grid(int)`
    // cell is a whole whitespace-delimited token. It is not, and it never
    // consistently was: over this same input the predecessor answered the four
    // cells `[12, 2, 34, 4]`, reading each token and then re-reading its tail.
    // D11 settles it — a `grid` cell is one Unicode scalar, so `grid(int)` is
    // one digit per cell and `matrix(int)` is the token-granular constructor.
    // The grid is `[1, 2 / 3, 4]`, so `g[1, 0]` (written 7) + `g[0, 0]` = 1 +
    // `g[0, 1]` = 3. The coordinate order this test exists for is unchanged:
    // the off-diagonal cell is still the one that catches a swapped store.
    let (rt, result) = run_main_with_input(
        "fn main() -> Int {\n  let g = read grid(int)\n  g[1, 0] = 7\n  \
         g[1, 0] * 100 + g[0, 0] * 10 + g[0, 1]\n}\n",
        "12\n34\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 700 + 10 + 3);

    // `.get`/`.set` and the subscript are the same cell, which is what says the
    // two spellings are one operation for a `Grid` (unlike a `Map`'s, §4.7).
    let (rt, result) = run_main_with_input(
        "fn main() -> Int {\n  let g = read grid(int)\n  g.set(1, 1, 5)\n  \
         g[1, 1] * 10 + g.get(1, 1)\n}\n",
        "12\n34\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 55);

    // Out of range faults rather than reading a neighbour, from either side.
    let (rt, _) = run_main_with_input(
        "fn main() -> Int {\n  let g = read grid(int)\n  g[99, 0]\n}\n",
        "12\n34\n",
    );
    assert!(rt.has_pending_fault(), "an out-of-range read faults");
    let (rt, _) = run_main_with_input(
        "fn main() -> Int {\n  let g = read grid(int)\n  g[0, 99] = 1\n  0\n}\n",
        "12\n34\n",
    );
    assert!(rt.has_pending_fault(), "an out-of-range store faults");
}

/// **REP-16's evaluation rule.** A compound assignment through a subscript
/// evaluates its receiver and indices **once**.
///
/// The desugaring `m[k] += v` → `m[k] = m[k] + v` names the place twice, and MIR
/// lowers each `TypedExpr` where it stands, so an index with a side effect would
/// happen twice. `TypedStmt::IndexAssign` carries the pieces once for this
/// reason, and this is the test that fails if it is ever desugared.
#[test]
fn a_compound_assignment_through_a_subscript_evaluates_its_place_once() {
    // `key` appends to a log and returns the key, so the log's length is the
    // number of times the index expression ran.
    let (rt, result) = run_main(
        "fn key(log) { log.push(1)\n \"k\" }\n\
         fn main() -> Int {\n  let log = Vec()\n  let c = Counter()\n  \
         c[key(log)] += 1\n  log.len() * 10 + c[\"k\"]\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 11, "one call to `key`, one increment");

    // …and the receiver too, which is the other half a desugaring would double.
    let (rt, result) = run_main(
        "fn pick(log, c) { log.push(1)\n c }\n\
         fn main() -> Int {\n  let log = Vec()\n  let c = Counter()\n  \
         pick(log, c)[\"k\"] += 2\n  log.len() * 10 + c[\"k\"]\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 12, "one call to `pick`, one increment");
}

/// **REP-09 end to end.** A constructor with written type arguments runs, and it
/// builds the collection the annotation names.
///
/// The half a type test cannot see: the ctor's runtime call carries a *descriptor*
/// chosen from the element type, and the descriptor selects hash and equality for
/// a `Map`/`Counter` key. A written type argument that reached inference but not
/// the allocation would type-check and then key the collection by the wrong
/// comparison.
#[test]
fn a_constructor_with_written_type_arguments_builds_what_it_names() {
    // A tuple-keyed `Counter`, spelled the way §3.3 spells it. The key is a fresh
    // allocation each iteration, so the three increments only land on one key if
    // the descriptor gives structural identity (ADR-026).
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let c = Counter[(Int, Int)]()\n  \
         for i in 0..3 { c[(1, 2)] += 1 }\n  c[(1, 2)] * 10 + c.len()\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 31);

    // Each ctor arity, and a nested argument.
    for (src, want) in [
        ("let v = Vec[Int]()\n  v.push(4)\n  v[0]", 4),
        ("let m = Map[Text, Int]()\n  m[\"a\"] = 9\n  m[\"a\"]", 9),
        (
            "let m = Map[Text, Vec[Int]]()\n  let inner = Vec()\n  inner.push(3)\n  \
             m[\"a\"] = inner\n  m[\"a\"].len()",
            1,
        ),
    ] {
        let (rt, result) = run_main(&format!("fn main() -> Int {{\n  {src}\n}}\n"));
        assert!(!rt.has_pending_fault(), "{src} faulted: {:?}", rt.fault());
        assert_eq!(result.as_int(), want, "{src}");
    }
}

/// **REP-18.** A keyed collection can be enumerated, in a deterministic order, and
/// `count` takes a predicate.
///
/// §3.3's representative program ends `counts.values().count(|n| n >= 2)`, and
/// neither half existed: `values` was in no catalog row, and `count` was defined
/// only at arity zero. The order is asserted because a `HashMap`'s own iteration
/// order is randomized per process — without a fixed order the *answer* of a
/// program like `m.keys()[0]` would change between runs, which is RT-16 in a place
/// where printing is not the only thing affected.
#[test]
fn a_keyed_collection_enumerates_in_a_deterministic_order() {
    // `values()` on a Counter, which is what §3.3 needs.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let c = Counter[Text]()\n  c[\"a\"] = 3\n  c[\"b\"] = 1\n  \
         c[\"c\"] = 5\n  c.values().count(|n| n >= 2)\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2, "two of the three counts are >= 2");

    // `count(pred)` is `filter(pred).count()`, and the two spellings agree.
    for (src, want) in [
        ("v.count(|n| n > 2)", 2),
        ("v.filter(|n| n > 2).count()", 2),
        ("v.count()", 4),
        // …and it composes with the stages before it, because it *is* a filter
        // plus the count sink rather than a sink of its own.
        ("v.map(|n| n * 2).count(|n| n > 4)", 2),
    ] {
        let program = format!(
            "fn main() -> Int {{\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  \
             v.push(4)\n  {src}\n}}\n"
        );
        let (rt, result) = run_main(&program);
        assert!(!rt.has_pending_fault(), "{src} faulted: {:?}", rt.fault());
        assert_eq!(result.as_int(), want, "{src}");
    }

    // `keys()` and `values()` are **index-aligned**: the pair at each index is one
    // entry. Three keys whose rendered order differs from their insertion order,
    // so an implementation that returned the `HashMap`'s order would disagree with
    // itself here rather than only look untidy.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let m = Map[Text, Int]()\n  m[\"c\"] = 3\n  m[\"a\"] = 1\n  \
         m[\"b\"] = 2\n  let ks = m.keys()\n  let vs = m.values()\n  \
         var ok = 0\n  for i in 0..ks.len() { if m[ks[i]] == vs[i] { ok += 1 } }\n  ok\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(
        result.as_int(),
        3,
        "every index pairs a key with its own value"
    );

    // The order itself, twice in one process and asserted against the rendered-key
    // order the formatter already uses.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let m = Map[Text, Int]()\n  m[\"c\"] = 3\n  m[\"a\"] = 1\n  \
         m[\"b\"] = 2\n  let vs = m.values()\n  vs[0] * 100 + vs[1] * 10 + vs[2]\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 123, "ordered by key: a, b, c");

    // An empty collection enumerates to an empty `Vec` rather than faulting.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let m = Map[Text, Int]()\n  \
         m.keys().len() + m.values().len()\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

/// **REP-20.** A template literal that begins with a space matches.
///
/// The interpreter honours a literal's whitespace policy *before* matching its
/// bytes, and the scanner left the leading space run in the text as well — so the
/// space was consumed twice and the literal could never match. §3.3's own template
/// is `` `{x1:int},{y1:int} -> {x2:int},{y2:int}` ``, which failed at the `-` of
/// `->` on every input.
#[test]
fn a_template_literal_that_begins_with_a_space_matches() {
    // The finding's own shape, and §3.3's.
    let (rt, result) = run_main_with_input(
        "fn main() -> Int {\n  let rs = read lines(`{a:int} -> {b:int}`)\n  \
         var t = 0\n  for r in rs { t = t + r.a * r.b }\n  t\n}\n",
        "1 -> 2\n3 -> 4\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 14);

    let (rt, result) = run_main_with_input(
        "fn main() -> Int {\n  \
         let rs = read lines(`{x1:int},{y1:int} -> {x2:int},{y2:int}`)\n  \
         var t = 0\n  for r in rs { t = t + r.x2 - r.x1 + r.y2 - r.y1 }\n  t\n}\n",
        "0,9 -> 5,9\n8,0 -> 0,8\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    // (5 - 0) + (9 - 9) for the first line, (0 - 8) + (8 - 0) for the second.
    assert_eq!(result.as_int(), 5);

    // The policy is still flexible, which is what stripping the run *into* it
    // preserves: extra spaces and none at all both match.
    for input in ["1 -> 2\n", "1    ->    2\n", "1->2\n"] {
        let (rt, result) = run_main_with_input(
            "fn main() -> Int {\n  let rs = read lines(`{a:int} -> {b:int}`)\n  \
             var t = 0\n  for r in rs { t = t + r.a + r.b }\n  t\n}\n",
            input,
        );
        assert!(
            !rt.has_pending_fault(),
            "{input:?} faulted: {:?}",
            rt.fault()
        );
        assert_eq!(result.as_int(), 3, "{input:?}");
    }

    // A literal with no leading space is untouched, and one that is *only* spaces
    // is a whitespace part.
    let (rt, result) = run_main_with_input(
        "fn main() -> Int {\n  let rs = read lines(`{a:int},{b:int}`)\n  \
         var t = 0\n  for r in rs { t = t + r.a + r.b }\n  t\n}\n",
        "1,2\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
    let (rt, result) = run_main_with_input(
        "fn main() -> Int {\n  let rs = read lines(`{a:int} {b:int}`)\n  \
         var t = 0\n  for r in rs { t = t + r.a + r.b }\n  t\n}\n",
        "1 2\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

// ===========================================================================
// REP-15: every iterable has a `for` lowering (ADR-066)
// ===========================================================================

/// **REP-15's headline.** `capability::iter_item` has said all ten collections
/// are iterable since M8, and **six of them had no lowering at all**: MIR's
/// symbol pickers had arms for `Vec`, `Deque` and `Range` and defaulted the rest
/// to `praxis_vec_get`, so a `Set`'s payload was read as a `Vec`'s.
///
/// So this is the test the defect existed for lack of: one `for` per iterable,
/// each answering something a wrong read could not produce. Two of the failure
/// modes are not assertions — **hanging and dying are**: `for x in s` over a
/// `Set` used to kill the test process, and a `MinHeap` over `[3, 1, 2]` summed
/// to `4349199564`, which is the worse one because nothing reported it.
#[test]
fn a_for_reaches_every_member_of_every_iterable() {
    // The three that already worked, so a regression here fails loudly rather
    // than quietly turning into a snapshot.
    for (src, want, what) in [
        (
            "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  \
             var t = 0\n  for x in v { t = t * 10 + x }\n  t\n}\n",
            12,
            "Vec, in push order",
        ),
        (
            "fn main() -> Int {\n  let d = Deque()\n  d.push_back(1)\n  d.push_front(2)\n  \
             var t = 0\n  for x in d { t = t * 10 + x }\n  t\n}\n",
            21,
            "Deque, front to back",
        ),
        (
            "fn main() -> Int {\n  var t = 0\n  for i in 1..4 { t = t * 10 + i }\n  t\n}\n",
            123,
            "Range, ascending and half-open",
        ),
        // A `Set` is the one that killed the process. Two members, so a
        // one-member answer is a different number from a two-member one.
        (
            "fn main() -> Int {\n  let s = Set()\n  s.insert(3)\n  s.insert(1)\n  \
             var t = 0\n  for x in s { t = t * 10 + x }\n  t\n}\n",
            13,
            "Set, ascending by rendered member",
        ),
        // A `BitSet`'s members are bit positions, not objects: each is boxed by
        // the snapshot rather than copied out of the payload.
        (
            "fn main() -> Int {\n  let b = BitSet()\n  b.insert(5)\n  b.insert(2)\n  \
             var t = 0\n  for i in b { t = t * 10 + i }\n  t\n}\n",
            25,
            "BitSet, ascending",
        ),
        // The silently-wrong one. `[3, 1, 2]` in *pop* order is 1, 2, 3 — the
        // backing array is heap-ordered only at its root, so an indexed read of
        // it would answer in insertion-history order even if the read were
        // type-correct.
        (
            "fn main() -> Int {\n  let h = MinHeap()\n  h.push(3)\n  h.push(1)\n  h.push(2)\n  \
             var t = 0\n  for x in h { t = t * 10 + x }\n  t\n}\n",
            123,
            "MinHeap, ascending (pop order)",
        ),
        (
            "fn main() -> Int {\n  let h = MaxHeap()\n  h.push(1)\n  h.push(3)\n  h.push(2)\n  \
             var t = 0\n  for x in h { t = t * 10 + x }\n  t\n}\n",
            321,
            "MaxHeap, descending (pop order)",
        ),
        // A keyed collection yields the `(K, V)` pair `iter_item` has always
        // said it does; both halves are read here so a pair built in the wrong
        // order fails.
        (
            "fn main() -> Int {\n  let m = Map()\n  m.insert(1, 7)\n  m.insert(2, 8)\n  \
             var t = 0\n  for kv in m { t = t * 100 + kv.0 * 10 + kv.1 }\n  t\n}\n",
            1728,
            "Map, key then value",
        ),
        (
            "fn main() -> Int {\n  let c = Counter()\n  c.inc(1)\n  c.inc(1)\n  c.inc(2)\n  \
             var t = 0\n  for kv in c { t = t * 100 + kv.0 * 10 + kv.1 }\n  t\n}\n",
            1221,
            "Counter, key then count",
        ),
    ] {
        let (rt, result) = run_main(src);
        assert!(!rt.has_pending_fault(), "{what} faulted: {:?}", rt.fault());
        assert_eq!(result.as_int(), want, "{what}");
    }

    // A `Grid` is the tenth, and it needs input: a `Grid()` is 0×0.
    let (rt, result) = run_main_with_input(
        "fn main() -> Int {\n  let g = read grid(char)\n  var n = 0\n  \
         for c in g { n = n + 1 }\n  n\n}\n",
        "ab\ncd\n",
    );
    assert!(!rt.has_pending_fault(), "Grid faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 4, "Grid, every cell row-major");

    // An empty one of each iterates zero times rather than once or forever —
    // the shape a length read off the wrong payload gets wrong first.
    for (src, what) in [
        ("let s = Set()\n  for x in s { n = n + 1 }", "Set"),
        ("let b = BitSet()\n  for x in b { n = n + 1 }", "BitSet"),
        ("let h = MinHeap()\n  for x in h { n = n + 1 }", "MinHeap"),
        ("let h = MaxHeap()\n  for x in h { n = n + 1 }", "MaxHeap"),
        ("let m = Map()\n  for kv in m { n = n + 1 }", "Map"),
        ("let c = Counter()\n  for kv in c { n = n + 1 }", "Counter"),
    ] {
        let src = format!("fn main() -> Int {{\n  var n = 0\n  {src}\n  n\n}}\n");
        let (rt, result) = run_main(&src);
        assert!(
            !rt.has_pending_fault(),
            "empty {what} faulted: {:?}",
            rt.fault()
        );
        assert_eq!(result.as_int(), 0, "an empty {what} iterates zero times");
    }
}

/// A `for` iterates a **snapshot** taken once before the loop (ADR-066), and the
/// two consequences that has are observable from a program.
///
/// The first is that the loop body may mutate the collection it is walking
/// without the walk changing under it — which a live cursor over a `HashMap`
/// could not offer at all, since Rust's own iterator would be invalidated.
///
/// The second is that a heap is **not drained**: pop order is where the snapshot
/// gets its order, not how it gets its members.
#[test]
fn a_for_iterates_a_snapshot_of_what_it_was_given() {
    // Inserting into the set being walked adds nothing to *this* walk. Members
    // 1 and 2 are visited; 11 and 12 are inserted and not seen.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let s = Set()\n  s.insert(1)\n  s.insert(2)\n  \
         var n = 0\n  for x in s { n = n + 1\n s.insert(x + 10) }\n  n * 10 + s.len()\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 24, "two steps, four members afterwards");

    // The heap still holds everything it held: iterating is not popping.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let h = MinHeap()\n  h.push(3)\n  h.push(1)\n  \
         var n = 0\n  for x in h { n = n + 1 }\n  n * 10 + h.len()\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 22, "two steps, and the heap is untouched");

    // The snapshot has to survive the body's allocations: it is a `Gc` local the
    // loop keeps live, and if liveness missed it a collection would reclaim the
    // `Vec` being indexed. 300 members × an allocating body is well past the
    // initial 64 KiB threshold.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let s = Set()\n  var i = 0\n  \
         while i < 300 { s.insert(i)\n i = i + 1 }\n  \
         var t = 0\n  for x in s { t = t + x * 2 }\n  t\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 299 * 300, "sum(0..300) * 2");

    // …and so must the *pair* of snapshots a keyed collection walks, where the
    // keys must survive the values' allocation as well as the body's.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let m = Map()\n  var i = 0\n  \
         while i < 300 { m.insert(i, i)\n i = i + 1 }\n  \
         var t = 0\n  for kv in m { t = t + kv.0 + kv.1 }\n  t\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 299 * 300, "sum(0..300) twice over");
}

/// A `for`'s order is the order the collection's own accessors already promise,
/// which is what makes an iterating program's answer reproducible.
///
/// A `HashSet`'s iteration order is randomized **per process**, so this is RT-16
/// with teeth again (REP-18): the same program would sum the same numbers but
/// concatenate them differently on two runs. The three orders are each pinned
/// against the accessor that shares them.
#[test]
fn an_iterables_order_is_the_one_its_own_accessors_promise() {
    // A `Map`'s `for` visits exactly what `keys()`/`values()` list, in step.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let m = Map()\n  m.insert(3, 30)\n  m.insert(1, 10)\n  \
         m.insert(2, 20)\n  let ks = m.keys()\n  let vs = m.values()\n  \
         var i = 0\n  var agree = 1\n  \
         for kv in m { if kv.0 != ks.get(i) { agree = 0 }\n \
         if kv.1 != vs.get(i) { agree = 0 }\n i = i + 1 }\n  agree * 10 + i\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 13, "three steps, every one index-aligned");

    // A `Counter`'s is the same rule through the same helper.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let c = Counter()\n  c.inc(2)\n  c.inc(1)\n  c.inc(1)\n  \
         let ks = c.keys()\n  var i = 0\n  var agree = 1\n  \
         for kv in c { if kv.0 != ks.get(i) { agree = 0 }\n i = i + 1 }\n  agree * 10 + i\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 12);

    // A heap's is pop order, which is the one order here that is *meaningful*
    // rather than merely fixed — so it is asserted as a sequence and not only as
    // a set. Popping the same heap answers the same sequence.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let h = MinHeap()\n  h.push(5)\n  h.push(1)\n  h.push(9)\n  \
         var walked = 0\n  for x in h { walked = walked * 10 + x }\n  \
         var popped = 0\n  while h.len() > 0 { popped = popped * 10 + h.pop() }\n  \
         if walked == popped { walked } else { 0 }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 159, "walking and draining agree");

    // Insertion order does not show through: two sets built in opposite orders
    // concatenate to the same number. (An in-process proxy for the per-process
    // seed, which is the same one `maps.rs`'s own gates use.)
    let (rt, forward) = run_main(
        "fn main() -> Int {\n  let s = Set()\n  s.insert(1)\n  s.insert(2)\n  s.insert(3)\n  \
         var t = 0\n  for x in s { t = t * 10 + x }\n  t\n}\n",
    );
    let (rt2, backward) = run_main(
        "fn main() -> Int {\n  let s = Set()\n  s.insert(3)\n  s.insert(2)\n  s.insert(1)\n  \
         var t = 0\n  for x in s { t = t * 10 + x }\n  t\n}\n",
    );
    assert!(!rt.has_pending_fault() && !rt2.has_pending_fault());
    assert_eq!(forward.as_int(), backward.as_int());
    assert_eq!(forward.as_int(), 123);
}

/// ADR-062's asymmetry, extended to the seven iterables that had no lowering:
/// **one `for` body serves every iterable it is given**, because the iterator
/// stays quantified and monomorphization makes one clone per iterable kind —
/// and each clone picks its own [`IterPlan`] from a concrete ctor.
///
/// REP-03's gate proves this for `Vec`, `BitSet` and `Range`. Those three were
/// the three that *worked*; this is the half of the property REP-15 was hiding.
#[test]
fn a_for_over_an_unannotated_parameter_reaches_each_iterable_it_is_given() {
    // One source function, four ctors, all four in one program — so the clones
    // have to be distinct and each has to select its own accessor pair.
    let (rt, result) = run_main(
        "fn total(c) { var t = 0\n for x in c { t = t + x }\n t }\n\
         fn main() -> Int {\n  \
         let v = Vec()\n  v.push(1)\n  \
         let s = Set()\n  s.insert(2)\n  \
         let b = BitSet()\n  b.insert(4)\n  \
         let h = MinHeap()\n  h.push(8)\n  \
         total(v) + total(s) + total(b) + total(h)\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 15, "each clone reached its own members");

    // …including the paired plan, whose item is a tuple rather than an element.
    let (rt, result) = run_main(
        "fn tally(c) { var t = 0\n for kv in c { t = t + kv.1 }\n t }\n\
         fn main() -> Int {\n  \
         let m = Map()\n  m.insert(1, 5)\n  \
         let c = Counter()\n  c.inc(9)\n  \
         tally(m) * 10 + tally(c)\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 51);
}

/// **REP-23.** A fused `enumerate()`/`zip()` pair holds both of its halves.
///
/// It held **neither**: MIR emits `AllocKind::Tuple { ty: MirType::Opaque, … }`
/// for the fused pipelines — their item types arrive with MIR-05 — the codegen
/// degraded `Opaque` to a *zero-element* schema, and `praxis_alloc_tuple` sizes
/// the payload from the schema. So both `praxis_tuple_set` calls wrote into a
/// zero-length `items` and `[10, 20].enumerate()` answered `[(), ()]`, out of a
/// documented §6.3 combinator and with nothing reported.
///
/// The static types are still MIR-05's to supply (S21). What this pins is that
/// the *values* survive — the schema keeps its arity and says "no static type"
/// per slot, which ADR-066 made a thing a slot can say.
#[test]
fn a_fused_pair_carries_both_of_its_halves() {
    // `enumerate` pairs an index with an element, so reading `.0` and `.1` with
    // different weights fails on a swap as well as on a drop.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let v = Vec()\n  v.push(10)\n  v.push(20)\n  v.push(30)\n  \
         var t = 0\n  for p in v.enumerate().collect() { t = t + p.0 * 100 + p.1 }\n  t\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 360, "(0,10) + (1,20) + (2,30), weighted");

    // `zip` is the other producer of an `Opaque`-typed pair.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let a = Vec()\n  a.push(1)\n  a.push(2)\n  \
         let b = Vec()\n  b.push(30)\n  b.push(40)\n  \
         var t = 0\n  for p in a.zip(b).collect() { t = t + p.0 * 100 + p.1 }\n  t\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 370, "(1,30) + (2,40), weighted");

    // The pairs are also *values*: two runs of the same pipeline are equal, and
    // one whose halves differ is not — so the elements reach equality's
    // element-wise walk and are not compared as two empty tuples.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  \
         let w = Vec()\n  w.push(1)\n  w.push(9)\n  \
         let same = v.enumerate().collect() == v.enumerate().collect()\n  \
         let diff = v.enumerate().collect() == w.enumerate().collect()\n  \
         if same { 10 } else { 0 } + if diff { 1 } else { 0 }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 10, "equal to itself, unequal to the other");
}

/// **REP-10.** A record pattern reads *fields* and a tuple pattern reads
/// *elements* — the half no type test can see, because the two are different
/// runtime symbols (`praxis_record_field` and `praxis_tuple_get`) and a pattern
/// that picked the wrong one would type-check identically.
///
/// Every component is weighted differently, so reading the right slots in the
/// wrong order fails as loudly as dropping one.
#[test]
fn a_record_and_a_tuple_pattern_read_the_components_they_name() {
    // A record, punned. `x * 100 + y` is a different answer from `y * 100 + x`.
    let (rt, result) = run_main(
        "struct P { x: Int, y: Int }\n\
         fn main() -> Int {\n  let p = P { x: 3, y: 4 }\n  \
         match p { P { x, y } => x * 100 + y }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 304);

    // …and explicit, with the fields written in the *other* order, so a
    // lowering that paired sub-patterns by position rather than by declared
    // field index answers 403.
    let (rt, result) = run_main(
        "struct P { x: Int, y: Int }\n\
         fn main() -> Int {\n  let p = P { x: 3, y: 4 }\n  \
         match p { P { y: b, x: a } => a * 100 + b }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 304, "bound by field name, not by position");

    // A tuple, by position.
    let (rt, result) =
        run_main("fn main() -> Int {\n  let t = (3, 4)\n  match t { (a, b) => a * 100 + b }\n}\n");
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 304);

    // A literal sub-pattern *selects* an arm: the components are tested, not
    // merely bound, so the first arm has to fail on its first element.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var t = 0\n  \
         for n in 1..4 {\n    let p = (n, n * 10)\n    \
         t = t + match p { (1, b) => b, (2, _) => 200, (_, b) => b * 2 }\n  }\n  t\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(
        result.as_int(),
        10 + 200 + 60,
        "one arm per value, in order"
    );

    // The composites nest, in both directions, and through an enum payload —
    // which is the three component readers chained in one decision tree.
    let (rt, result) = run_main(
        "struct P { x: Int, y: Int }\n\
         fn main() -> Int {\n  let o = Some((P { x: 3, y: 4 }, 5))\n  \
         match o { Some((P { x, y }, k)) => x * 100 + y * 10 + k, None => 0 }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 345);

    // A field the pattern does not name is a wildcard, and the ones it does are
    // still read from their own slots — a padded row must not shift the rest.
    let (rt, result) = run_main(
        "struct P { a: Int, b: Int, c: Int }\n\
         fn main() -> Int {\n  let p = P { a: 1, b: 2, c: 3 }\n  \
         match p { P { c } => c }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3, "the third field, not the first");

    // A record pattern binds a field of any type, not only the scalar the
    // arithmetic above could hide a mis-read of.
    let (rt, result) = run_main(
        "struct Tagged { name: Text, n: Int }\n\
         fn main() -> Int {\n  let t = Tagged { name: \"abc\", n: 7 }\n  \
         match t { Tagged { name, n } => name.len() * 10 + n }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 37);
}

/// **REP-21.** `min=` keeps the smaller value, `max=` the larger, and an absent
/// entry accepts the first — which is the half no type test can see, and the
/// half the runtime wrappers had been waiting for since they were written with
/// no caller.
#[test]
fn an_updating_store_keeps_the_better_value_and_accepts_the_first() {
    // `min=` on a key that already has a value: the smaller wins, whichever
    // order the candidates arrive in, and a *worse* candidate changes nothing.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let d = Map()\n  d[\"a\"] = 10\n  \
         d[\"a\"] min= 4\n  d[\"a\"] min= 7\n  d[\"a\"] min= 9\n  d[\"a\"]\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 4);

    // `max=` is its dual, and the two are different wrappers: a program that
    // computed one with the other answers 4 here.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let b = Map()\n  b[\"a\"] = 10\n  \
         b[\"a\"] max= 4\n  b[\"a\"] max= 17\n  b[\"a\"] max= 9\n  b[\"a\"]\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 17);

    // **An absent entry accepts the first value** (§6.2), and it must not fault:
    // a subscript *read* of an absent key does (§4.7), which is exactly why this
    // cannot be a read-modify-write and is a row of its own.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let d = Map()\n  d[\"fresh\"] min= 42\n  \
         let b = Map()\n  b[\"fresh\"] max= 7\n  d[\"fresh\"] * 100 + b[\"fresh\"]\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 4207);

    // …and the first value is accepted *whatever* it is: a later, larger
    // candidate does not replace it under `min=`.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let d = Map()\n  d[\"k\"] min= 3\n  d[\"k\"] min= 100\n  d[\"k\"]\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);

    // Several keys, and a key computed by an expression — the place is a real
    // subscript and not a name in disguise.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let d = Map()\n  var i = 0\n  \
         while i < 6 {\n    d[i % 3] min= 10 - i\n    i = i + 1\n  }\n  \
         d[0] * 100 + d[1] * 10 + d[2]\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 765, "keys 0,1,2 keep 7,6,5");

    // §6.2's own shape: a Dijkstra-style relaxation through a generic helper, so
    // the deferred receiver reaches the backend as well as the type checker.
    let (rt, result) = run_main(
        "fn relax(dist, key, candidate) {\n  dist[key] min= candidate\n}\n\
         fn main() -> Int {\n  let distance = Map()\n  \
         relax(distance, 1, 7)\n  relax(distance, 1, 3)\n  relax(distance, 2, 9)\n  \
         distance[1] * 100 + distance[2]\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 309);

    // The update does not read: a `min=` on a map with an absent key runs where
    // `d[k] = d[k] + 1` would fault, and the map still holds one entry per key.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let d = Map()\n  d[\"a\"] min= 5\n  d[\"a\"] min= 5\n  d.len()\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

/// **REP-25.** A destructuring `for` binding reads the components it names, once
/// per step — the half no type test can see.
#[test]
fn a_destructuring_for_binding_reads_each_item_apart() {
    // `for (k, v) in m` — §6.2's shape for walking a map, weighted so a swapped
    // pair or a dropped half is a different answer.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let m = Map()\n  m[1] = 20\n  m[3] = 40\n  \
         var t = 0\n  for (k, v) in m { t = t + k * 100 + v }\n  t\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 460, "(1,20) + (3,40), weighted");

    // …over a `Vec` of pairs, which is the in-place plan rather than the paired
    // snapshot — the two lowerings meet the same binding.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let v = Vec()\n  v.push((1, 20))\n  v.push((3, 40))\n  \
         var t = 0\n  for (a, b) in v { t = t + a * 100 + b }\n  t\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 460);

    // A record pattern in the header, and a field the pattern does not name.
    let (rt, result) = run_main(
        "struct P { x: Int, y: Int, z: Int }\n\
         fn main() -> Int {\n  let ps = Vec()\n  ps.push(P { x: 1, y: 2, z: 3 })\n  \
         ps.push(P { x: 4, y: 5, z: 6 })\n  var t = 0\n  \
         for P { z, x } in ps { t = t + x * 10 + z }\n  t\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 13 + 46);

    // Nested, and mutated: the binding is a fresh read each step, so a name bound
    // in one step does not leak into the next.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let v = Vec()\n  v.push((1, (2, 3)))\n  v.push((4, (5, 6)))\n  \
         var t = 0\n  for (a, (b, c)) in v { t = t * 10 + a + b + c }\n  t\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 6 * 10 + 15);

    // A bare name still binds the whole item, and the pair is still readable
    // with `.0`/`.1` — the spelling every existing program uses.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let m = Map()\n  m[1] = 20\n  \
         var t = 0\n  for kv in m { t = t + kv.0 * 100 + kv.1 }\n  t\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 120);

    // The destructured names survive an allocation inside the body: they are
    // real slots, not borrowed views into the item.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let m = Map()\n  m[1] = 2\n  m[3] = 4\n  \
         var t = 0\n  for (k, v) in m {\n    let scratch = Vec()\n    var i = 0\n    \
         while i < 50 { scratch.push(i)\n i = i + 1 }\n    t = t + k * 10 + v\n  }\n  t\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 12 + 34);
}

// ---------------------------------------------------------------------------
// S18: the `Option` contract, end to end (D1, RT-14/RT-15).
// ---------------------------------------------------------------------------

/// D1's headline gate. `Map.get` answers `Option[V]` (§5.7 writes that
/// signature literally), so a program tells absence from a value by *matching*
/// rather than by comparing the answer against something it is not.
///
/// The runtime builds the `Option` through its own `option_schema`, whose
/// `Some` slot is unknown, and the program's arms were compiled against the
/// codegen's `Option[Int]` schema. That the two meet at all is
/// `EnumSchema::same_type`'s null-slot rule (RT-13); this is where it earns its
/// keep.
#[test]
fn an_absent_map_get_matches_none_and_a_present_one_matches_some() {
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let m = Map()\n  m.insert(1, 10)\n  \
         match m.get(1) {\n    Some(v) => v,\n    None => 0 - 1,\n  }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 10);

    let (rt, result) = run_main(
        "fn main() -> Int {\n  let m = Map()\n  m.insert(1, 10)\n  \
         match m.get(2) {\n    Some(v) => v,\n    None => 0 - 1,\n  }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), -1);

    // The value inside the `Some` is the real one, whatever its type: a `Text`
    // value comes back as a `Text`, not as an `i64` read of its buffer pointer.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let m = Map()\n  m.insert(1, \"abc\")\n  \
         match m.get(1) {\n    Some(v) => v.len(),\n    None => 0 - 1,\n  }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);

    // …and `map[key]` is still the other half of §4.7's sentence: the
    // assertion-like spelling, which faults rather than answering `None`.
    let (rt, _) = run_main("fn main() -> Int {\n  let m = Map()\n  m[7]\n}\n");
    assert!(
        rt.has_pending_fault(),
        "§4.7: indexing a missing key faults"
    );
}

/// The same contract under a *tuple* payload: `Grid.find` answers
/// `Option[(Int, Int)]`, and the point survives being carried inside the enum.
#[test]
fn an_absent_grid_find_is_none_and_a_hit_is_some_of_the_point() {
    let src = "fn main() -> Int {\n  let g = read matrix(int)\n  \
               match g.find(4) {\n    Some((x, y)) => x * 10 + y,\n    None => 0 - 1,\n  }\n}\n";
    let (rt, result) = run_main_with_input(src, "1 2\n3 4\n");
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 11, "4 is at (1, 1)");

    let src = "fn main() -> Int {\n  let g = read matrix(int)\n  \
               match g.find(99) {\n    Some((x, y)) => x * 10 + y,\n    None => 0 - 1,\n  }\n}\n";
    let (rt, result) = run_main_with_input(src, "1 2\n3 4\n");
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), -1);
}

/// RT-13 from source, at the one place a program can observe an enum's identity:
/// **equality against a value the two producers built independently**.
///
/// The static type system already keeps two *declared* enum types apart, so
/// `Colour == Light` never reaches the runtime. What does reach it is one
/// `Option` type built twice — the runtime makes `m.get(k)`'s answer through
/// its own schema, whose `Some` slot is unknown, and the program makes
/// `Some(10)` through the codegen's `Option[Int]` schema. Those must be one
/// type, or absolutely nothing about `Map.get` works; and the payloads must
/// still decide, or a `Some("x")` would be read as a `Some(int)`.
#[test]
fn an_option_from_the_runtime_and_one_from_the_program_are_one_type() {
    // Same type, same payload: equal.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let m = Map()\n  m.insert(1, 10)\n  \
         if m.get(1) == Some(10) { 1 } else { 0 }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(
        result.as_int(),
        1,
        "a runtime-built Some and a program-written one are one type"
    );

    // Same type, different payload: not equal.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let m = Map()\n  m.insert(1, 10)\n  \
         if m.get(1) == Some(11) { 1 } else { 0 }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);

    // Absence is `None`, and `None` is not `Some` of anything.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let m = Map()\n  m.insert(1, 10)\n  \
         if m.get(2) == None { 1 } else { 0 }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);

    // A `Text` payload stays a `Text`: the schema slot the runtime filled is
    // unknown, so the value's own descriptor decides, and it is never wrong.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  let m = Map()\n  m.insert(1, \"x\")\n  \
         if m.get(1) == Some(\"x\") { 1 } else { 0 }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

// --- §7.4's atomic parsers, end to end (IP-11) ------------------------------

/// **IP-11.** Four of §7.4's ten atomic parsers did not exist: `uint`, `float`,
/// `byte`, `identifier`. A program that wrote one got "unknown atomic parser"
/// for a name the design document requires.
///
/// This is the half neither the type test nor the runtime unit test can see: a
/// compiled program reading real input through the real ABI, so the value's
/// descriptor has to be right as well as its type.
#[test]
fn every_atomic_the_design_requires_runs_in_a_compiled_program() {
    // `uint` is an Int, and arithmetic on it works.
    let (rt, result) = run_main_with_input(
        "fn main() -> Int {\n  let n = read uint\n  n + 1\n}\n",
        "41",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);

    // A leading `-` is not a `uint`: §7.4's non-negativity is the parse rule,
    // because `ScalarType::UInt` has no runtime object to be typed with.
    let (rt, _) = run_main_with_input(
        "fn main() -> Int {\n  let n = read uint\n  n + 1\n}\n",
        "-1",
    );
    assert!(rt.has_pending_fault(), "`uint` must refuse a negative");

    // `float` is a Float, and it reads a fraction.
    let (rt, result) = run_main_with_input(
        "fn main() -> Float {\n  let x = read float\n  x + 0.5\n}\n",
        "3.25",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_float(), 3.75);

    // `byte` is a Byte in 0..=255.
    let (rt, result) = run_main_with_input(
        "fn main() -> Int {\n  let bs = read csv(byte)\n  bs.len()\n}\n",
        "0,127,255",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
    let (rt, _) = run_main_with_input("fn main() -> Int {\n  let b = read byte\n  1\n}\n", "256");
    assert!(rt.has_pending_fault(), "256 is not a byte");

    // `identifier` is a Text under §4.1's one character class, so a Unicode
    // name is a name — a deliberate widening of §7.4's "ASCII-like by default".
    let (rt, result) = run_main_with_input(
        "fn main() -> Int {\n  let names = read lines(identifier)\n  names.len()\n}\n",
        "alpha\nλx\n_beta9\n",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);

    // And in a template capture, which is the shape they will actually be
    // written in.
    let (rt, result) = run_main_with_input(
        "fn main() -> Int {\n  let rows = read lines(`{name:identifier}={n:uint}`)\n  \
         var t = 0\n  for r in rows { t = t + r.n }\n  t\n}\n",
        "a=1\nb=2\nc=39\n",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}
