//! End-to-end JIT integration tests: source → typed HIR → MIR → Cranelift → run.
//!
//! The acceptance tests for §19: execute boxed integer arithmetic, branches,
//! loops, and recursive function calls through the JIT, and confirm faults
//! return to the host without unwinding.

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
    // Monomorphization: instantiate polymorphic callees per call site.
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
/// table. This walks the MIR of a feature-broad program and checks each callee
/// by name.
#[test]
fn every_runtime_symbol_mir_emits_is_registered() {
    let src = concat!(
        "struct P { x: Int, y: Int }\n",
        "enum E { A, B(Int) }\n",
        "fn main() -> Int {\n",
        "  var v = Vec()\n  v.push(1)\n  v.push(2)\n",
        "  var m = Map()\n  m.insert(\"k\", 1)\n",
        "  var s = Set()\n  s.insert(3)\n",
        "  var d = Deque()\n  d.push_back(4)\n",
        "  var c = Counter()\n  c.inc(5)\n",
        "  var t = (1, 2)\n",
        "  var p = P { x: 1, y: 2 }\n",
        "  var e = B(7)\n",
        "  var f = |z| z + 1\n",
        "  var b = BitSet()\n  b.insert(6)\n",
        "  var mh = MinHeap()\n  mh.push(7)\n",
        "  var xh = MaxHeap()\n  xh.push(8)\n",
        "  var acc = 0\n",
        "  for x in v { acc = acc + f(x) }\n",
        // Every snapshot symbol `IterPlan` can select — a `for` is the only
        // caller of four of them, so nothing else would reach them here.
        "  for x in s { acc = acc + x }\n",
        "  for x in b { acc = acc + x }\n",
        "  for x in mh { acc = acc + x }\n",
        "  for x in xh { acc = acc + x }\n",
        "  for kv in m { acc = acc + kv.1 }\n",
        "  for kv in c { acc = acc + kv.1 }\n",
        "  var txt = \"hi\"\n",
        "  out(txt.len())\n",
        // A `Text` is the eleventh iterable, and the only one that is not a
        // collection: its plan names `praxis_text_len`/`praxis_text_get`.
        "  for ch in txt { acc = acc + ch.to_int() }\n",
        // A list literal, which emits a `Vec` allocation plus one
        // `praxis_vec_push` per element.
        "  for x in [1, 2] { acc = acc + x }\n",
        "  var fl = 1.5\n  out(fl.sqrt())\n",
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

    // Every symbol codegen emits — named in MIR or not — must resolve.
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
/// as a Text buffer (§6.3; guarded in `praxis_get_input`).
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

/// A digit separator is punctuation, so it changes no value.
///
/// Lowering strips the `_`s and parses what is left. Both positions are covered:
/// an expression and a pattern, which read the token through two different
/// decoders.
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
            "fn main() -> Int { var n = 1_000\n match n { 1_000 => 7, _ => 0 } }",
            7,
        ),
        (
            "fn main() -> Int { var n = 1000\n match n { 1_0_0 => 1, 1_000 => 7, _ => 0 } }",
            7,
        ),
    ] {
        let (rt, result) = run_main(src);
        assert!(!rt.has_pending_fault(), "{src} faulted");
        assert_eq!(result.as_int(), want, "{src}");
    }

    // A float's fraction and exponent are separated too.
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
    let src = "fn main() -> Float { var a = 1.5; var b = 2.0; var c = 3.0; a * b * c }";
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
    let (rt, result) = run_main("fn main() -> Bool { var x = 0.0/0.0; x == x }");
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
    // §6.2: unbounded recursion must fault gracefully instead of overflowing the
    // native stack and aborting the host — §9.2/§17.4 require the host to
    // survive. Every generated function's prologue reads `ctx.stack_left` inline
    // and branches to a stack-overflow fault epilogue when what is left of the
    // native-stack budget will not cover this frame, raising
    // FaultKind::StackOverflow and unwinding to the host; only if it passes does
    // it spend the budget and push.
    //
    // `count` is a minimum-width frame, so ADR-105's byte budget lets it recurse
    // exactly MAX_RECURSION_DEPTH (8000) deep. For the *wide* frame see
    // `adr105_a_wide_frame_faults_where_a_reference_frame_does_not`.
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
// The classic integer corner cases: the asymmetry around MIN, the
// division-overflow trap (MIN / -1 raises SIGFPE on x86 if not guarded), modulo
// sign/overflow, and negation overflow.

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

/// §4.12's three explicit overflow alternatives exist and opt out of the fault:
/// `a.wrapping_add(b)`, `a.saturating_add(b)`, and `a.checked_add(b)`, which
/// returns `Option[Int]`.
///
/// Every assertion is at `i64::MAX`, where the ordinary `+` faults: a test that
/// added two small numbers would pass against `praxis_int_add` itself and prove
/// only that a name resolves.
#[test]
fn the_overflow_alternatives_answer_where_a_checked_add_faults() {
    // MAX = 9223372036854775807. Wrapping lands on MIN; adding MAX back and one
    // more brings it to 0, which is a number the harness can compare.
    let src = "fn main() -> Int {\n  var m = 9223372036854775807\n  \
               m.wrapping_add(1) + m + 1\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "wrapping_add must not fault");
    assert_eq!(result.as_int(), 0, "MAX.wrapping_add(1) is MIN");

    // Saturating stays at MAX, so subtracting MAX is zero.
    let src = "fn main() -> Int {\n  var m = 9223372036854775807\n  \
               m.saturating_add(1) - m\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "saturating_add must not fault");
    assert_eq!(result.as_int(), 0, "MAX.saturating_add(1) is MAX");

    // `checked_add` answers an `Option[Int]` — a real one (ADR-076), so a
    // `match` reaches inside it. `None` on overflow, `Some` below it.
    let src = "fn main() -> Int {\n  var m = 9223372036854775807\n  \
               match m.checked_add(1) { Some(n) => n\n None => 7 }\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "checked_add must not fault");
    assert_eq!(result.as_int(), 7, "MAX.checked_add(1) is None");

    let src = "fn main() -> Int {\n  match 5.checked_add(7) { Some(n) => n\n None => 0 }\n}";
    let (_rt, result) = run_main(src);
    assert_eq!(result.as_int(), 12, "a sum that fits is Some(sum)");
}

/// The `_sub` trio, at the boundary where `-` faults.
///
/// Like its `_add` sibling, every assertion is at `Int::MIN`, where the ordinary
/// operator faults — two small numbers would pass against `praxis_int_sub`
/// itself and prove only that a name resolves. Values **and** the absence of a
/// pending fault, because an alternative that faults is not an alternative.
///
/// `Int::MIN` is spelled `-9223372036854775807 - 1`: `9223372036854775808` is
/// not an `Int` literal, and unary minus binds after the literal.
#[test]
fn the_sub_alternatives_answer_where_a_checked_sub_faults() {
    // MIN.wrapping_sub(1) is MAX, so subtracting MAX is 0.
    let src = "fn main() -> Int {\n  var m = -9223372036854775807 - 1\n  \
               m.wrapping_sub(1) - 9223372036854775807\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "wrapping_sub must not fault");
    assert_eq!(result.as_int(), 0, "MIN.wrapping_sub(1) is MAX");

    // Saturating stays at MIN, so subtracting MIN is 0.
    let src = "fn main() -> Int {\n  var m = -9223372036854775807 - 1\n  \
               m.saturating_sub(1) - m\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "saturating_sub must not fault");
    assert_eq!(result.as_int(), 0, "MIN.saturating_sub(1) is MIN");

    let src = "fn main() -> Int {\n  var m = -9223372036854775807 - 1\n  \
               match m.checked_sub(1) { Some(n) => n\n None => 7 }\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "checked_sub must not fault");
    assert_eq!(result.as_int(), 7, "MIN.checked_sub(1) is None");

    let src = "fn main() -> Int {\n  match 12.checked_sub(7) { Some(n) => n\n None => 0 }\n}";
    let (_rt, result) = run_main(src);
    assert_eq!(result.as_int(), 5, "a difference that fits is Some(it)");
}

/// The `_mul` trio. `wrapping_mul` has *no in-language spelling*: every
/// arithmetic operator is checked and there are no bitwise operators, so this
/// method is the only way to reach modular multiplication.
///
/// `2^62 * 4` is `2^64`, i.e. exactly 0 under two's-complement wraparound. That
/// is a better probe than a near-boundary product because the wrapped answer is
/// a value no partial computation would land on by accident.
#[test]
fn the_mul_alternatives_answer_where_a_checked_mul_faults() {
    let src = "fn main() -> Int {\n  4611686018427387904.wrapping_mul(4)\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "wrapping_mul must not fault");
    assert_eq!(result.as_int(), 0, "2^62 * 4 wraps to exactly 0");

    // Saturating clamps to MAX, so subtracting MAX is 0.
    let src = "fn main() -> Int {\n  \
               4611686018427387904.saturating_mul(4) - 9223372036854775807\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "saturating_mul must not fault");
    assert_eq!(result.as_int(), 0, "2^62 * 4 saturates to MAX");

    let src = "fn main() -> Int {\n  \
               match 4611686018427387904.checked_mul(4) { Some(n) => n\n None => 7 }\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "checked_mul must not fault");
    assert_eq!(result.as_int(), 7, "2^62 * 4 is None");

    let src = "fn main() -> Int {\n  match 6.checked_mul(7) { Some(n) => n\n None => 0 }\n}";
    let (_rt, result) = run_main(src);
    assert_eq!(result.as_int(), 42, "a product that fits is Some(it)");
}

/// `Counter.inc` is arithmetic, and §4.12's arithmetic is checked.
///
/// A `Counter`'s values are ordinary `Int`s a program can store with
/// `c[k] = n`, so the ceiling is reachable from source. The wrapper's `cur + 1`
/// must fault rather than panic inside `#[no_mangle] extern "C"` (the
/// non-unwinding panic §10.4 forbids) or wrap silently: per-wrapper totality
/// makes faulting the only available behaviour.
///
/// The fault must be *observed*, which is why the assertion is `rt.fault()` and
/// not merely "the program did not crash": marking the wrapper checked without
/// marking its manifest row `AllocatesAndFaults` leaves the fault pending and
/// the generated code running, so the next unrelated `CheckFault` reports it at
/// the wrong place.
#[test]
fn counter_inc_at_the_int_ceiling_faults_rather_than_wrapping() {
    let src = "fn main() -> Int {\n  var c = Counter()\n  c[\"k\"] = 9223372036854775807\n  \
               c.inc(\"k\")\n  c[\"k\"]\n}";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "inc past i64::MAX should fault");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::IntOverflow);
}

/// The companion to the ceiling test above: an increment that fits is still an
/// increment and a fresh key still starts at one, so a "fix" that faulted on
/// every `inc` would pass that test and fail here.
#[test]
fn counter_inc_below_the_ceiling_still_counts() {
    let src = "fn main() -> Int {\n  var c = Counter()\n  c[\"k\"] = 9223372036854775806\n  \
               c.inc(\"k\")\n  c.inc(\"fresh\")\n  c[\"k\"] - 9223372036854775806 + c[\"fresh\"]\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "no overflow here");
    assert_eq!(result.as_int(), 2, "one increment on each key");
}

#[test]
fn adv_modulo_by_zero_faults() {
    // % 0 must fault as DivByZero.
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
    // Sanity: ordinary division produces the right result — the overflow check
    // must not reject a valid division.
    let src = "fn main() -> Int { 100 / 7 }";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 14);
}

// ===========================================================================
// Shadow-stack GC spill (ADR-019).
// ===========================================================================

#[test]
fn live_locals_survive_collection_during_a_loop() {
    // The headline spill test: a loop that allocates heavily (well past the
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
// Vec[T] method surface (§11, §16.2).
// ===========================================================================

#[test]
fn vec_push_and_len_end_to_end() {
    // The headline vertical slice: construct a Vec, push values, read len.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "no fault expected");
    assert_eq!(result.as_int(), 3);
}

#[test]
fn vec_get_reads_back_elements() {
    // Push 10, 20, 30; get index 1 → 20.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(10)\n  v.push(20)\n  v.push(30)\n  v.get(1)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 20);
}

#[test]
fn vec_get_out_of_bounds_faults() {
    // Accessing index 0 of an empty vector faults IndexOutOfBounds.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.get(0)\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "OOB should fault");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::IndexOutOfBounds);
}

#[test]
fn vec_push_many_with_collection_during_growth() {
    // Push 500 elements (forcing many GCs during growth), check length.
    // This exercises both the method surface and the shadow-stack spill: the
    // vector `v` must survive across every push's allocation + collection.
    let src = "fn main() -> Int {\n  var v = Vec()\n  var i = 0\n  while i < 500 { v.push(i); i = i + 1 }\n  v.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 500);
}

#[test]
fn vec_push_many_read_back_correct() {
    // Push 500 elements and read back the last (index 499 = value 499). This is
    // a stricter test of the shadow-stack spill: the vec's *contents* must
    // survive across every collection, not just the vec object itself.
    let src = "fn main() -> Int {\n  var v = Vec()\n  var i = 0\n  while i < 500 { v.push(i); i = i + 1 }\n  v.get(499)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 499);
}

// --- Vec[T]() construction honors the real element descriptor ---------------

#[test]
fn vec_of_vec_equality_after_construction() {
    // Two `Vec()`-constructed vectors holding identical inner vectors must be
    // structurally equal. This only works when `Vec[T]()` construction passes
    // the *real* element descriptor (the outer Vec's element descriptor must be
    // `VEC`, not the null/INT default), so nested equality dispatches through
    // `vec_equals` on the inner elements.
    let src = "fn main() -> Int {\n  var outer_a = Vec()\n  var inner_a = Vec()\n  inner_a.push(1)\n  inner_a.push(2)\n  outer_a.push(inner_a)\n  var outer_b = Vec()\n  var inner_b = Vec()\n  inner_b.push(1)\n  inner_b.push(2)\n  outer_b.push(inner_b)\n  if outer_a == outer_b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn vec_of_vec_inequality_after_construction() {
    // The complement: two `Vec()`-constructed vectors holding *different* inner
    // vectors must be structurally unequal. An element descriptor defaulting to
    // INT would compare only lengths or mis-dispatch, and could spuriously
    // report equal.
    let src = "fn main() -> Int {\n  var outer_a = Vec()\n  var inner_a = Vec()\n  inner_a.push(1)\n  inner_a.push(2)\n  outer_a.push(inner_a)\n  var outer_b = Vec()\n  var inner_b = Vec()\n  inner_b.push(1)\n  inner_b.push(9)\n  outer_b.push(inner_b)\n  if outer_a == outer_b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

// --- Deque[T] (§6.1) --------------------------------------------------------

#[test]
fn deque_push_back_and_len_end_to_end() {
    // Construct a Deque, push_back three values, read len → 3.
    let src = "fn main() -> Int {\n  var d = Deque()\n  d.push_back(1)\n  d.push_back(2)\n  d.push_back(3)\n  d.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

#[test]
fn deque_push_front_yields_fifo_order() {
    // push_front(1), push_front(2), push_front(3) → front-to-back is [3,2,1].
    // pop_front returns 3 (the last pushed), proving FIFO-from-front semantics.
    let src = "fn main() -> Int {\n  var d = Deque()\n  d.push_front(1)\n  d.push_front(2)\n  d.push_front(3)\n  d.pop_front()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

#[test]
fn deque_push_back_pop_front_is_fifo() {
    // push_back then pop_front is a classic FIFO queue: 1,2,3 in → 1 out first.
    let src = "fn main() -> Int {\n  var d = Deque()\n  d.push_back(1)\n  d.push_back(2)\n  d.push_back(3)\n  d.pop_front()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn deque_push_front_pop_back_is_lifo() {
    // push_front then pop_back is LIFO: 1,2,3 pushed to front → pop_back gives 1.
    let src = "fn main() -> Int {\n  var d = Deque()\n  d.push_front(1)\n  d.push_front(2)\n  d.push_front(3)\n  d.pop_back()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn deque_get_indexes_from_front() {
    // push_back 10,20,30 → get(0)=10, get(2)=30 (0-based from the front).
    let src = "fn main() -> Int {\n  var d = Deque()\n  d.push_back(10)\n  d.push_back(20)\n  d.push_back(30)\n  d.get(2)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 30);
}

#[test]
fn deque_pop_front_on_empty_faults() {
    // Popping an empty deque faults EmptyCollection.
    let src = "fn main() -> Int {\n  var d = Deque()\n  d.pop_front()\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "empty pop should fault");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::EmptyCollection);
}

#[test]
fn deque_pop_back_on_empty_faults() {
    let src = "fn main() -> Int {\n  var d = Deque()\n  d.pop_back()\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "empty pop should fault");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::EmptyCollection);
}

#[test]
fn deque_is_empty_true_then_false() {
    // An empty deque is_empty → 1; after a push it is not → 0.
    let src = "fn main() -> Int {\n  var d = Deque()\n  if d.is_empty() { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn deque_drained_is_empty() {
    // Push one, pop one → empty again.
    let src = "fn main() -> Int {\n  var d = Deque()\n  d.push_back(7)\n  var _ = d.pop_front()\n  if d.is_empty() { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn deque_equality_is_structural() {
    // Two deques with the same elements in the same order are equal.
    let src = "fn main() -> Int {\n  var a = Deque()\n  a.push_back(1)\n  a.push_back(2)\n  var b = Deque()\n  b.push_back(1)\n  b.push_back(2)\n  if a == b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

// --- Map[K,V] / Set[T] / Counter[T] (§6.1, §11.3) ---------------------------
// These are the headline §19.7 tests: tuples/records/nested collections as
// map/set keys, working end-to-end through the DynamicKey descriptor bridge.

#[test]
fn map_insert_get_len_end_to_end() {
    // Insert two (Int→Int) entries, get one back, check len.
    let src =
        "fn main() -> Int {\n  var m = Map()\n  m.insert(1, 10)\n  m.insert(2, 20)\n  m.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn map_get_returns_inserted_value() {
    let src = "fn main() -> Int {\n  var m = Map()\n  m.insert(7, 42)\n  m[7]\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

/// An absent `Map.get` answers `None`, and a present one answers `Some`.
///
/// §5.7 spells the signature `Map[K,V].get(K) -> Option[V]` and §4.7 makes
/// absence an `Option`, so the program can tell absence from a value.
#[test]
fn an_absent_map_get_answers_none_and_a_present_one_answers_some() {
    let unwrap = "fn unwrap(o: Option[Int]) -> Int {\n  match o {\n    Some(v) => v,\n    None => 0 - 1,\n  }\n}\n";
    let (rt, result) = run_main(&format!(
        "{unwrap}fn main() -> Int {{\n  var m = Map()\n  m.insert(1, 10)\n  unwrap(m.get(99))\n}}\n"
    ));
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), -1, "an absent key is `None`");

    let (rt, result) = run_main(&format!(
        "{unwrap}fn main() -> Int {{\n  var m = Map()\n  m.insert(1, 10)\n  unwrap(m.get(1))\n}}\n"
    ));
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 10, "a present key is `Some(value)`");
}

#[test]
fn map_contains_distinguishes_present_and_absent() {
    let src = "fn main() -> Int {\n  var m = Map()\n  m.insert(5, 1)\n  if m.contains(5) { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn map_remove_drops_entry() {
    let src = "fn main() -> Int {\n  var m = Map()\n  m.insert(1, 10)\n  m.insert(2, 20)\n  m.remove(1)\n  m.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn map_insert_overwrites_prior_value() {
    let src =
        "fn main() -> Int {\n  var m = Map()\n  m.insert(1, 10)\n  m.insert(1, 99)\n  m[1]\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 99);
}

#[test]
fn map_with_tuple_keys_end_to_end() {
    // The headline §19.7 criterion: tuples as map keys. Two structurally-equal
    // tuples must hit the same entry.
    let src = "fn main() -> Int {\n  var m = Map()\n  m.insert((1, 2), 100)\n  m[(1, 2)]\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 100);
}

#[test]
fn map_with_distinct_tuple_keys() {
    // (1,2) and (1,3) are distinct keys.
    let src = "fn main() -> Int {\n  var m = Map()\n  m.insert((1, 2), 100)\n  m.insert((1, 3), 200)\n  m.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn map_with_text_keys_end_to_end() {
    // Text keys: two equal strings hit the same entry.
    let src = "fn main() -> Int {\n  var m = Map()\n  m.insert(\"hello\", 1)\n  m[\"hello\"]\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn set_insert_contains_len_end_to_end() {
    let src = "fn main() -> Int {\n  var s = Set()\n  s.insert(1)\n  s.insert(2)\n  s.insert(1)\n  s.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // Duplicate insert (1 twice) → 2 distinct elements.
    assert_eq!(result.as_int(), 2);
}

#[test]
fn set_contains_true_false() {
    let src = "fn main() -> Int {\n  var s = Set()\n  s.insert(7)\n  if s.contains(7) { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn set_with_tuple_keys() {
    // Tuples in a set (§19.7).
    let src = "fn main() -> Int {\n  var s = Set()\n  s.insert((1, 2))\n  s.insert((1, 2))\n  s.insert((3, 4))\n  s.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn counter_absent_reads_zero() {
    // §6.2: "Counter missing values behave as zero" — the §19.8 acceptance
    // criterion. An absent key's count is 0.
    let src = "fn main() -> Int {\n  var c = Counter()\n  c.get(\"absent\")\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

#[test]
fn counter_inc_increments() {
    let src = "fn main() -> Int {\n  var c = Counter()\n  c.inc(\"a\")\n  c.inc(\"a\")\n  c.inc(\"a\")\n  c.get(\"a\")\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

#[test]
fn counter_distinct_keys_tracked_separately() {
    let src = "fn main() -> Int {\n  var c = Counter()\n  c.inc(\"a\")\n  c.inc(\"b\")\n  c.inc(\"b\")\n  c.get(\"b\")\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn counter_vec_sourced_text_keys_accumulate() {
    // Text keys sourced from a Vec (distinct allocations) accumulate in a
    // Counter via structural Text hashing.
    let src = "fn main() -> Int {\n  var words = Vec()\n  words.push(\"apple\")\n  words.push(\"apple\")\n  words.push(\"banana\")\n  var counts = Counter()\n  var i = 0\n  while i < words.len() {\n    counts.inc(words.get(i))\n    i = i + 1\n  }\n  counts.get(\"apple\")\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn counter_len_counts_distinct_keys() {
    let src = "fn main() -> Int {\n  var c = Counter()\n  c.inc(\"a\")\n  c.inc(\"a\")\n  c.inc(\"b\")\n  c.len()\n}\n";
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
    // Build a Vec of literal Texts and count each via a Counter: the second
    // occurrence of the same *value* must hit the existing entry even though it
    // is a distinct allocation (DynamicKey structural eq, not pointer identity).
    let src = "fn main() -> Int {\n  var words = Vec()\n  words.push(\"apple\")\n  words.push(\"apple\")\n  words.push(\"pear\")\n  var c = Counter()\n  var i = 0\n  while i < words.len() { c.inc(words.get(i)); i = i + 1 }\n  c.get(\"apple\")\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn adv_counter_text_keys_from_read_accumulate() {
    // The strongest form: Text keys sourced from `read` (source-slice
    // TextPayload, distinct from any literal). Count repeated words parsed from
    // input; equal values must aggregate.
    let src = "fn main() -> Int {\n  var words = read lines(word)\n  var c = Counter()\n  var i = 0\n  while i < words.len() { c.inc(words.get(i)); i = i + 1 }\n  c.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "apple\napple\npear\napple\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // 2 distinct values ("apple", "pear")
    assert_eq!(result.as_int(), 2);
}

#[test]
fn adv_counter_text_keys_from_read_get_count() {
    // As above but read back the count for "apple" (3 occurrences).
    let src = "fn main() -> Int {\n  var words = read lines(word)\n  var c = Counter()\n  var i = 0\n  while i < words.len() { c.inc(words.get(i)); i = i + 1 }\n  c.get(\"apple\")\n}\n";
    let (rt, result) = run_main_with_input(src, "apple\napple\npear\napple\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

#[test]
fn adv_map_text_key_distinct_alloc_lookup() {
    // Map insert with a literal Text key, then look up with a structurally-
    // equal Text from a different source (a Vec). Must find the entry via
    // structural eq, not pointer identity.
    let src = "fn main() -> Int {\n  var m = Map()\n  m.insert(\"hello\", 42)\n  var keys = Vec()\n  keys.push(\"hello\")\n  m[keys.get(0)]\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

#[test]
fn adv_map_text_key_from_read_lookup() {
    // Map keyed by source-slice Text from `read`. Insert all, then look up one
    // by a literal of equal value.
    let src = "fn main() -> Int {\n  var words = read lines(word)\n  var m = Map()\n  var i = 0\n  while i < words.len() { m.insert(words.get(i), i); i = i + 1 }\n  m[\"pear\"]\n}\n";
    let (rt, result) = run_main_with_input(src, "apple\npear\nkiwi\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // "pear" was inserted at index 1
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_set_text_key_distinct_alloc_contains() {
    // Set with a literal Text member; `contains` with a distinct-allocation
    // equal Text must return true via structural eq.
    let src = "fn main() -> Int {\n  var s = Set()\n  s.insert(\"hello\")\n  var keys = Vec()\n  keys.push(\"hello\")\n  var b = s.contains(keys.get(0))\n  if b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_set_dedupes_distinct_alloc_equal_text() {
    // Insert the same Text value twice (distinct allocations from a Vec); the
    // set must dedupe to one member (structural eq).
    let src = "fn main() -> Int {\n  var words = Vec()\n  words.push(\"x\")\n  words.push(\"x\")\n  words.push(\"y\")\n  var s = Set()\n  var i = 0\n  while i < words.len() { s.insert(words.get(i)); i = i + 1 }\n  s.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn adv_map_tuple_key_distinct_alloc() {
    // Tuple keys built from distinct allocations. Two (1,2) tuples from
    // different construction sites must map to the same entry.
    let src = "fn main() -> Int {\n  var m = Map()\n  m.insert((1, 2), 100)\n  var pairs = Vec()\n  pairs.push((1, 2))\n  m[pairs.get(0)]\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 100);
}

#[test]
fn adv_map_large_under_gc_pressure() {
    // Insert 500 entries under GC pressure, then look up a mid-range key.
    // Verifies map entries (keys + values) survive GC via map_trace.
    let src = "fn main() -> Int {\n  var m = Map()\n  var i = 0\n  while i < 500 { m.insert(i, i * 2); i = i + 1 }\n  m[250]\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 500);
}

#[test]
fn adv_set_large_under_gc_pressure() {
    // 500 set members under GC; contains must still find a mid-range one.
    let src = "fn main() -> Int {\n  var s = Set()\n  var i = 0\n  while i < 500 { s.insert(i); i = i + 1 }\n  var b = s.contains(499)\n  if b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_counter_large_under_gc_pressure() {
    // 500 distinct keys, each incremented once, then count distinct + one count.
    let src = "fn main() -> Int {\n  var c = Counter()\n  var i = 0\n  while i < 500 { c.inc(i); i = i + 1 }\n  c.get(300)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_map_overwrite_then_get() {
    // Overwriting an existing key's value must not duplicate the entry.
    let src = "fn main() -> Int {\n  var m = Map()\n  m.insert(\"k\", 1)\n  m.insert(\"k\", 2)\n  m.insert(\"k\", 3)\n  m.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

/// `None` is a value the program can *name*: §4.7 makes absence an `Option`, so
/// a `match` reads the answer directly instead of probing it for
/// not-being-a-value.
#[test]
fn an_absent_map_get_is_a_none_the_program_can_match_on() {
    let src = "fn main() -> Int {\n  var m = Map()\n  m.insert(\"a\", 1)\n  match m.get(\"missing\") {\n    Some(v) => v,\n    None => 7,\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 7, "the `None` arm ran");

    // And a `Some` binds the value rather than merely being "not Unit".
    let src = "fn main() -> Int {\n  var m = Map()\n  m.insert(\"a\", 1)\n  match m.get(\"a\") {\n    Some(v) => v,\n    None => 7,\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_map_index_of_a_missing_key_faults_where_get_answers() {
    // §4.7's own sentence, both halves in one test: "indexing a missing map key
    // faults instead of returning an option… the user chooses between explicit
    // absence with `.get` and assertion-like access with indexing".
    let src = "fn main() -> Int {\n  var m = Map()\n  m.insert(\"a\", 1)\n  var v = m[\"missing\"]\n  if v == 0 { 1 } else { 0 }\n}\n";
    let (rt, _) = run_main(src);
    assert!(
        rt.has_pending_fault(),
        "§4.7: indexing a missing key faults"
    );

    // …and `.get` on the same absent key does not, so the fault is the
    // subscript's choice and not the map's. §4.7 says `.get` answers an
    // `Option`, so the arm is what the test reads.
    let src = "fn main() -> Int {\n  var m = Map()\n  m.insert(\"a\", 1)\n  match m.get(\"missing\") {\n    Some(v) => v,\n    None => 0,\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0, "explicit absence, not a fault");

    // A present key is the value, through the subscript.
    let src = "fn main() -> Int {\n  var m = Map()\n  m.insert(\"a\", 7)\n  m[\"a\"]\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 7);
}

// --- MinHeap[T] / MaxHeap[T] (§6.1, §11.2) ----------------------------------

#[test]
fn max_heap_pop_returns_largest() {
    // Push 3, 1, 2; pop yields 3 (the largest first).
    let src = "fn main() -> Int {\n  var h = MaxHeap()\n  h.push(3)\n  h.push(1)\n  h.push(2)\n  h.pop()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

#[test]
fn max_heap_pop_ordering_is_descending() {
    // Pop all three: 3, 2, 1 (descending).
    let src = "fn main() -> Int {\n  var h = MaxHeap()\n  h.push(3)\n  h.push(1)\n  h.push(2)\n  var a = h.pop()\n  var b = h.pop()\n  var c = h.pop()\n  a * 100 + b * 10 + c\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 321);
}

#[test]
fn max_heap_peek_does_not_remove() {
    let src = "fn main() -> Int {\n  var h = MaxHeap()\n  h.push(5)\n  h.push(10)\n  var _ = h.peek()\n  h.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn max_heap_peek_returns_largest() {
    let src = "fn main() -> Int {\n  var h = MaxHeap()\n  h.push(7)\n  h.push(3)\n  h.push(9)\n  h.peek()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 9);
}

#[test]
fn max_heap_pop_empty_faults() {
    let src = "fn main() -> Int {\n  var h = MaxHeap()\n  h.pop()\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "empty pop should fault");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::EmptyCollection);
}

#[test]
fn max_heap_is_empty_true() {
    let src = "fn main() -> Int {\n  var h = MaxHeap()\n  if h.is_empty() { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn min_heap_pop_returns_smallest() {
    // Push 3, 1, 2; pop yields 1 (the smallest first).
    let src = "fn main() -> Int {\n  var h = MinHeap()\n  h.push(3)\n  h.push(1)\n  h.push(2)\n  h.pop()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn min_heap_pop_ordering_is_ascending() {
    // Pop all three: 1, 2, 3 (ascending).
    let src = "fn main() -> Int {\n  var h = MinHeap()\n  h.push(3)\n  h.push(1)\n  h.push(2)\n  var a = h.pop()\n  var b = h.pop()\n  var c = h.pop()\n  a * 100 + b * 10 + c\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 123);
}

#[test]
fn min_heap_peek_returns_smallest() {
    let src = "fn main() -> Int {\n  var h = MinHeap()\n  h.push(7)\n  h.push(3)\n  h.push(9)\n  h.peek()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

#[test]
fn min_heap_pop_empty_faults() {
    let src = "fn main() -> Int {\n  var h = MinHeap()\n  h.pop()\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "empty pop should fault");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::EmptyCollection);
}

// --- BitSet (§6.1) ----------------------------------------------------------

#[test]
fn bitset_insert_contains_len() {
    let src = "fn main() -> Int {\n  var b = BitSet()\n  b.insert(0)\n  b.insert(64)\n  b.insert(1000)\n  b.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

#[test]
fn bitset_contains_true_false() {
    let src = "fn main() -> Int {\n  var b = BitSet()\n  b.insert(5)\n  if b.contains(5) { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn bitset_contains_absent_false() {
    let src = "fn main() -> Int {\n  var b = BitSet()\n  b.insert(5)\n  if b.contains(6) { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

#[test]
fn bitset_remove_clears_bit() {
    let src = "fn main() -> Int {\n  var b = BitSet()\n  b.insert(5)\n  b.insert(10)\n  b.remove(5)\n  b.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn bitset_is_empty_true_then_false() {
    let src = "fn main() -> Int {\n  var b = BitSet()\n  var first = if b.is_empty() { 1 } else { 0 }\n  b.insert(1)\n  var second = if b.is_empty() { 1 } else { 0 }\n  first * 10 + second\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 10);
}

// --- Grid[T] methods (§6.4) -------------------------------------------------

#[test]
fn grid_width_height_from_parsed_grid() {
    // Parse a 2-column × 2-row grid; width=2, height=2.
    let src = "fn main() -> Int {\n  var g = read grid(char)\n  g.width() * 10 + g.height()\n}\n";
    let (rt, result) = run_main_with_input(src, "ab\ncd\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 22);
}

#[test]
fn grid_get_reads_cell() {
    // Grid "ab/cd": get(1, 0) returns the Char 'b'. Compare via find_all: the
    // count of cells equal to the (1,0) cell should be 1.
    let src = "fn main() -> Int {\n  var g = read grid(char)\n  var cell = g.get(1, 0)\n  var matches = g.find_all(cell)\n  matches.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "ab\ncd\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn grid_get_out_of_bounds_faults() {
    let src = "fn main() -> Int {\n  var g = read grid(char)\n  var _ = g.get(9, 9)\n  0\n}\n";
    let (rt, _result) = run_main_with_input(src, "ab\n");
    assert!(rt.has_pending_fault(), "OOB should fault");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::IndexOutOfBounds);
}

#[test]
fn grid_contains_in_and_out() {
    // (1,1) is in a 2×2 grid; (5,5) is not.
    let src = "fn main() -> Int {\n  var g = read grid(char)\n  var a = if g.contains(1, 1) { 1 } else { 0 }\n  var b = if g.contains(5, 5) { 1 } else { 0 }\n  a * 10 + b\n}\n";
    let (rt, result) = run_main_with_input(src, "ab\ncd\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 10);
}

#[test]
fn grid_neighbors4_corner() {
    // Top-left corner (0,0) of a 2×2 grid has 2 in-bounds neighbors (right, down).
    let src = "fn main() -> Int {\n  var g = read grid(char)\n  var ns = g.neighbors4((0, 0))\n  ns.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "ab\ncd\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn grid_neighbors8_center() {
    // Center (1,1) of a 3×3 grid has all 8 neighbors.
    let src = "fn main() -> Int {\n  var g = read grid(char)\n  var ns = g.neighbors8((1, 1))\n  ns.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "abc\ndef\nghi\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 8);
}

#[test]
fn grid_positions_count() {
    // A 2×3 grid has 6 positions.
    let src =
        "fn main() -> Int {\n  var g = read grid(char)\n  var ps = g.positions()\n  ps.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "abc\ndef\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 6);
}

#[test]
fn grid_cells_count() {
    let src =
        "fn main() -> Int {\n  var g = read grid(char)\n  var cs = g.cells()\n  cs.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "ab\ncd\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 4);
}

#[test]
fn grid_row() {
    // Row 1 of "ab/cd" is "cd" (length 2). The row is a Vec[Char]; check its len.
    let src = "fn main() -> Int {\n  var g = read grid(char)\n  var r = g.row(1)\n  r.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "ab\ncd\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn grid_column() {
    // Column 0 of "ab/cd" is "ac" (length 2).
    let src =
        "fn main() -> Int {\n  var g = read grid(char)\n  var c = g.column(0)\n  c.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "ab\ncd\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn grid_find_locates_first_match() {
    // Grid "ab/cd": find a cell, then verify find_all for that cell finds 1.
    let src = "fn main() -> Int {\n  var g = read grid(char)\n  var cell = g.get(0, 1)\n  var matches = g.find_all(cell)\n  matches.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "ab\ncd\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn grid_find_all_count() {
    // Grid with two 'x' cells. Get the 'x' value via get(0,0) then find_all.
    let src = "fn main() -> Int {\n  var g = read grid(char)\n  var x = g.get(0, 0)\n  var matches = g.find_all(x)\n  matches.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "x.\n.x\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn grid_transpose_round_trips_dimensions() {
    // A 3-wide × 2-tall grid transposes to 2-wide × 3-tall.
    let src = "fn main() -> Int {\n  var g = read grid(char)\n  var t = g.transpose()\n  t.width() * 10 + t.height()\n}\n";
    let (rt, result) = run_main_with_input(src, "abc\ndef\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 23);
}

#[test]
fn grid_rotate_left_changes_dimensions() {
    // A 3-wide × 2-tall grid rotated left → 2-wide × 3-tall.
    let src = "fn main() -> Int {\n  var g = read grid(char)\n  var r = g.rotate_left()\n  r.width() * 10 + r.height()\n}\n";
    let (rt, result) = run_main_with_input(src, "abc\ndef\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 23);
}

#[test]
fn grid_rotate_right_changes_dimensions() {
    let src = "fn main() -> Int {\n  var g = read grid(char)\n  var r = g.rotate_right()\n  r.width() * 10 + r.height()\n}\n";
    let (rt, result) = run_main_with_input(src, "abc\ndef\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 23);
}

/// The two `*_changes_dimensions` tests above cannot tell the two rotations
/// apart — a left and a right rotation of a 3×2 grid are both 2×3 — and neither
/// can `grid_rotate_four_times_is_identity`, which is the identity whichever
/// direction it turns. So this reads a **cell**.
///
/// `abc / def` is asymmetric in both axes, so no transpose, flip or 180° turn
/// answers the same pair. With §6.4's convention (x rightward, y downward),
/// turning counter-clockwise carries the rightmost column to the top row:
/// `rotate_left()` is `cf / be / ad`, so its (0, 0) is the original (2, 0).
/// Turning clockwise carries the leftmost column to the top row reversed:
/// `rotate_right()` is `da / eb / fc`, so its (0, 0) is the original (0, 1).
/// Both halves are asserted in one answer, because an implementation that
/// swapped the two bodies would pass either half alone.
#[test]
fn grid_rotate_left_and_right_turn_in_opposite_directions() {
    let src = "fn main() -> Int {\n  var g = read grid(char)\n  var l = g.rotate_left()\n  var r = g.rotate_right()\n  var n = 0\n  if l.get(0, 0) == g.get(2, 0) { n = n + 1 }\n  if r.get(0, 0) == g.get(0, 1) { n = n + 10 }\n  n\n}\n";
    let (rt, result) = run_main_with_input(src, "abc\ndef\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 11);
}

/// The composition half: turning left then right restores the *contents*, not
/// merely the dimensions. This holds even when both directions are wrong in
/// the same way, so it complements the test above rather than replacing it.
#[test]
fn grid_rotate_left_then_right_restores_the_contents() {
    let src = "fn main() -> Int {\n  var g = read grid(char)\n  var back = g.rotate_left().rotate_right()\n  if g == back { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main_with_input(src, "abc\ndef\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn grid_rotate_four_times_is_identity() {
    // Rotating right 4× returns to the original dimensions (3×2).
    let src = "fn main() -> Int {\n  var g = read grid(char)\n  var r1 = g.rotate_right()\n  var r2 = r1.rotate_right()\n  var r3 = r2.rotate_right()\n  var r4 = r3.rotate_right()\n  r4.width() * 10 + r4.height()\n}\n";
    let (rt, result) = run_main_with_input(src, "abc\ndef\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 32); // back to 3-wide × 2-tall
}

// --- Control flow §4.11 (for/loop/break/continue/return) --------------------

#[test]
fn for_loop_sums_vec_elements() {
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(10)\n  v.push(20)\n  v.push(30)\n  var sum = 0\n  for x in v { sum = sum + x }\n  sum\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 60);
}

#[test]
fn for_loop_empty_vec_zero_iterations() {
    let src = "fn main() -> Int {\n  var v = Vec()\n  var sum = 0\n  for x in v { sum = sum + x }\n  sum\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

#[test]
fn for_loop_counts_iterations() {
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(1)\n  v.push(1)\n  v.push(1)\n  var n = 0\n  for x in v { n = n + 1 }\n  n\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 4);
}

/// A closure that is *itself the callee* of a call captures its `var` by cell
/// like any other, so the mutation outlives the call. Escape analysis has to
/// visit `Call.callee_expr` for that: an unboxed `count` would give each
/// increment its own copy.
#[test]
fn an_immediately_invoked_closure_mutates_the_var_it_captured() {
    let src = concat!(
        "fn main() -> Int {\n",
        "  var count = 0\n",
        "  var a = (|n| { count = count + n\n  count })(1)\n",
        "  var b = (|n| { count = count + n\n  count })(10)\n",
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

/// A `loop`'s value is the one its `break` carried, and it has to survive HIR
/// lowering, MIR and codegen — inference agreeing is not enough.
#[test]
fn expression_loop_returns_the_value_its_break_carried() {
    // The loop is the function's tail: nothing else can supply the answer.
    let src =
        "fn main() -> Int {\n  var i = 0\n  loop { i = i + 1 if i == 5 { break i * 2 } }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 10);

    // …and the value flows onward like any other: bound, then used.
    let src = "fn main() -> Int {\n  var i = 0\n  var found = loop { i = i + 1 if i * i > 30 { break i } }\n  found + 100\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 106, "6 * 6 = 36 is the first over 30");
}

/// A `loop` no `break` leaves is `Never`, and `Never` has no runtime
/// representation — so such a loop must not ask for a result slot, whose
/// descriptor site would fail the compile. Compiling and running the other
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

/// `continue` inside a `for` must advance the index: it targets the increment
/// block, not the loop header, or the loop never terminates.
#[test]
fn continue_in_a_for_loop_still_advances_the_index() {
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  var seen = 0\n  for x in v { if x == 2 { continue } seen = seen + x }\n  seen\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 4, "1 + 3, with 2 skipped");
}

#[test]
fn return_exits_function_early() {
    let src = "fn first(v: Vec[Int]) -> Int { for x in v { return x } 0 }\n  fn main() -> Int {\n  var v = Vec()\n  v.push(42)\n  v.push(99)\n  first(v)\n}\n";
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
    let src = "fn main() -> Int {\n  var d = Deque()\n  d.push_back(5)\n  d.push_back(10)\n  d.push_back(15)\n  var sum = 0\n  for x in d { sum = sum + x }\n  sum\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 30);
}

// --- Pipeline combinators (§6.3) --------------------------------------------

#[test]
fn pipeline_sum_sums_elements() {
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(10)\n  v.push(20)\n  v.push(30)\n  v.sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 60);
}

#[test]
fn pipeline_count_counts_elements() {
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.count()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

#[test]
fn pipeline_map_applies_closure() {
    // map (|x| x*2) over [1,2,3] → [2,4,6], then sum → 12.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  var doubled = v.map(|x| x * 2)\n  doubled.sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 12);
}

#[test]
fn pipeline_filter_keeps_matching() {
    // filter (|x| even) over [1,2,3,4] → [2,4], sum → 6.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  var evens = v.filter(|x| x - x / 2 * 2 == 0)\n  evens.sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 6);
}

/// **ADR-126.** A chain that ends on a stage materializes with nothing written
/// to say so — the `Collect` sink the recognizer appends is the whole of it.
///
/// A stage is what puts the binding on the sink's path, so a stage is what this
/// binds through.
#[test]
fn pipeline_materializes_without_a_written_sink() {
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  var doubled = v.map(|x| x * 2)\n  doubled.len() * 1000 + doubled.sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // 3 elements, and `[2, 4, 6]` rather than three of anything else — a `len`
    // alone passes on a Vec that materialized the wrong values.
    assert_eq!(result.as_int(), 3012);
}

#[test]
fn pipeline_map_then_len_chains() {
    // map then .len(): a method chain after a method-with-args.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.map(|x| x * 2).len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

// --- Cross-combinator fusion (§6.3) -----------------------------------------
// These exercise the single-fused-loop path: every multi-stage chain must
// produce the same value as the eager equivalent, with zero intermediate Vecs.

#[test]
fn pipeline_map_filter_sum_fuses() {
    // [1,2,3,4].map(*2)=[2,4,6,8].filter(even)=[2,4,6,8].sum()=20. (All doubled
    // values are even, so filter keeps all four.) One fused loop.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.map(|x| x * 2).filter(|x| x - x / 2 * 2 == 0).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 20);
}

#[test]
fn pipeline_filter_map_sum_fuses() {
    // [1,2,3,4,5].filter(odd)=[1,3,5].map(*10)=[10,30,50].sum()=90.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.push(5)\n  v.filter(|x| x - x / 2 * 2 == 1).map(|x| x * 10).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 90);
}

#[test]
fn pipeline_map_map_sum_fuses() {
    // [1,2,3].map(+1)=[2,3,4].map(*10)=[20,30,40].sum()=90.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.map(|x| x + 1).map(|x| x * 10).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 90);
}

#[test]
fn pipeline_filter_filter_sum_fuses() {
    // [1..6].filter(>2)=[3,4,5].filter(<5)=[3,4].sum()=7.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.push(5)\n  v.filter(|x| x > 2).filter(|x| x < 5).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 7);
}

#[test]
fn pipeline_three_stage_map_filter_map_sum() {
    // [1..6].map(+1)=[2..6].filter(even)=[2,4,6].map(*3)=[6,12,18].sum()=36.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.push(5)\n  v.map(|x| x + 1).filter(|x| x - x / 2 * 2 == 0).map(|x| x * 3).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 36);
}

#[test]
fn pipeline_chain_with_capturing_closure() {
    // Capturing closure in a fused chain.
    // var k = 10; [1..5].map(+k)=[11..14].filter(>13)=[14].sum()=14.
    let src = "fn main() -> Int {\n  var k = 10\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.map(|x| x + k).filter(|x| x > 13).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 14);
}

#[test]
fn pipeline_map_filter_count_fuses() {
    // [1..6].map(*2).filter(>5)=[6,8,10].count()=3.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.push(5)\n  v.map(|x| x * 2).filter(|x| x > 5).count()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

#[test]
fn pipeline_map_filter_collect_len() {
    // Fused chain ending in collect → len. [1..5].map(*2).filter(>4)=[6,8].len()=2.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  var out = v.map(|x| x * 2).filter(|x| x > 4)\n  out.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn pipeline_fused_chain_survives_gc_stress() {
    // 300 elements through a fused map+filter+sum. Verifies every fused stage
    // roots its live GcRefs across the collections the loop triggers.
    let src = "fn main() -> Int {\n  var v = Vec()\n  var i = 0\n  while i < 300 { v.push(i); i = i + 1 }\n  v.map(|x| x * 2).filter(|x| x > 100).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // The loop pushes i = 0..299 (300 elements). Sum of 2*i for i in 51..=299
    // (since 2*i > 100 ⟺ i > 50): 2 * (sum(1..=299) - sum(1..=50))
    // = 2 * (44850 - 1275) = 2 * 43575 = 87150.
    assert_eq!(result.as_int(), 87150);

    // The same stress over stages that own a *dense counter*. Each counter is a
    // `Gc` Int slot live across every `praxis_vec_get` safepoint in the loop,
    // exactly like the source cursor, so a root set that did not cover them
    // would hand the collector a stale word — and a slot the liveness pass
    // misses is nulled rather than merely stale.
    let src = "fn main() -> Int {\n  var v = Vec()\n  var i = 0\n  while i < 300 { v.push(i); i = i + 1 }\n  var t = 0\n  for p in v.filter(|x| x > 100).enumerate().take(3) { t = t + p.0 * 1000 + p.1 }\n  t\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // Filtered is 101..=299; enumerate numbers it densely from zero; take(3)
    // keeps (0,101), (1,102), (2,103) → 101 + 1102 + 2103 = 3306.
    assert_eq!(result.as_int(), 3306);
}

#[test]
fn pipeline_fold_threads_accumulator() {
    // [1..4].fold(100, |a,x| a - x) = 100-1-2-3 = 94.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.fold(100, |a, x| a - x)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 94);
}

#[test]
fn pipeline_fold_in_fused_chain() {
    // [1..4].map(*2)=[2,4,6].fold(0,|a,x|a+x)=12.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.map(|x| x * 2).fold(0, |a, x| a + x)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 12);
}

#[test]
fn pipeline_product_multiplies() {
    // [2,3,4].product() = 24.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.product()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 24);
}

#[test]
fn pipeline_reduce_seeds_from_first() {
    // [3,1,2].reduce(|a,x| if a<x then a else x) — but Praxis closures can't
    // branch by returning different values without an if-expression. Use a
    // simpler reducer: |a,x| a*10 + x → 3*10+1=31, 31*10+2=312.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(3)\n  v.push(1)\n  v.push(2)\n  v.reduce(|a, x| a * 10 + x)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 312);
}

#[test]
fn pipeline_min_finds_smallest() {
    // [5,2,8,1,9].min() = 1.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(5)\n  v.push(2)\n  v.push(8)\n  v.push(1)\n  v.push(9)\n  v.min()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn pipeline_max_finds_largest() {
    // [5,2,8,1,9].max() = 9.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(5)\n  v.push(2)\n  v.push(8)\n  v.push(1)\n  v.push(9)\n  v.max()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 9);
}

#[test]
fn pipeline_min_after_map_fuses() {
    // [1,5,2].map(*2)=[2,10,4].min()=2.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(5)\n  v.push(2)\n  v.map(|x| x * 2).min()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn pipeline_max_in_fused_chain() {
    // [1..5].filter(>2)=[3,4,5].max()=5.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.push(5)\n  v.filter(|x| x > 2).max()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 5);
}

#[test]
fn pipeline_any_true_when_one_matches() {
    // [1,2,3].any(|x| x == 2) = true → packed as 1.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  var b = v.any(|x| x == 2)\n  if b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn pipeline_any_false_when_none_match() {
    // [1,2,3].any(|x| x == 9) = false → packed as 0.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  var b = v.any(|x| x == 9)\n  if b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

#[test]
fn pipeline_all_true_when_all_match() {
    // [2,4,6].all(even) = true → 1.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(2)\n  v.push(4)\n  v.push(6)\n  var b = v.all(|x| x - x / 2 * 2 == 0)\n  if b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn pipeline_all_false_short_circuits() {
    // [2,4,5,6].all(even) = false (short-circuits at 5) → 0.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(2)\n  v.push(4)\n  v.push(5)\n  v.push(6)\n  var b = v.all(|x| x - x / 2 * 2 == 0)\n  if b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

/// §6.3 lists `find` and `position` as two operations: `find` answers the
/// matching *element*, `position` its index.
///
/// The vector is chosen so that only the right answer passes: `20` is at index
/// `1`, so a `find` that answered an index would answer `Some(1)`, which is not
/// `Some(20)`.
#[test]
fn find_answers_the_matching_element_not_its_index() {
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(10)\n  v.push(20)\n  v.push(30)\n  match v.find(|x| x == 20) { Some(n) => n, None => 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 20);
}

/// A miss is `None`, not a `-1` sentinel (ADR-082): `-1` is a legal element of a
/// `Vec[Int]` and a legal `Int` besides, so a sentinel could not tell a hit from
/// a miss. §4.7 says absence is `Option`.
#[test]
fn a_find_that_matches_nothing_answers_none() {
    // The miss arm answers 7, a number no element could produce, so a `Some`
    // sneaking through is visible rather than merely different.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(10)\n  v.push(20)\n  v.push(30)\n  match v.find(|x| x == 99) { Some(n) => n, None => 7 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 7);
}

/// `position` is not an alias of `find`: §6.3 describes two operations.
/// `position` keeps the index — that is its question — and answers it as an
/// `Option[Int]` for the same in-band-sentinel reason `find` does.
#[test]
fn position_answers_the_index_and_find_answers_the_element() {
    // `[10,20,30]`: `position(== 30)` is 2 and `find(== 30)` is 30. Summing the
    // two makes a swapped pair (30 + 2 the other way round) impossible to miss,
    // and a `-1` sentinel on either side lands nowhere near 32.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(10)\n  v.push(20)\n  v.push(30)\n  var p = match v.position(|x| x == 30) { Some(i) => i, None => 0 }\n  var f = match v.find(|x| x == 30) { Some(n) => n, None => 0 }\n  f * 10 + p\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 302);
}

/// `find` answers the receiver's element type, not `Int` (ADR-082): here the
/// elements are `Text`, so the result is an `Option[Text]`.
#[test]
fn find_reaches_a_non_int_element() {
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(\"alpha\")\n  v.push(\"beta\")\n  match v.find(|s| s == \"beta\") { Some(s) => s.len(), None => 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 4);
}

#[test]
fn pipeline_take_limits_elements() {
    // [1..5].take(3).sum() = 1+2+3 = 6.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.push(5)\n  v.take(3).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 6);
}

#[test]
fn pipeline_take_more_than_length() {
    // [1,2,3].take(10).sum() = 6 (take is bounded by length).
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.take(10).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 6);
}

#[test]
fn pipeline_skip_drops_prefix() {
    // [1..5].skip(2).sum() = 3+4+5 = 12.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.push(5)\n  v.skip(2).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 12);
}

#[test]
fn pipeline_take_then_map_then_sum() {
    // [1..5].take(3).map(*10)=[10,20,30].sum()=60.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.push(5)\n  v.take(3).map(|x| x * 10).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 60);
}

/// The bound of a `take`/`skip` is an `Int` expression, not an `Int` literal:
/// the catalog types the parameter `Int` and says nothing about literals, so the
/// recognizer must accept any expression there.
#[test]
fn a_take_or_skip_bound_is_any_int_expression() {
    let five = "  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.push(5)\n";
    let answer = |tail: &str| {
        let (rt, result) = run_main(&format!("fn main() -> Int {{\n{five}  {tail}\n}}\n"));
        assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
        result.as_int()
    };

    // A binding.
    assert_eq!(answer("var n = 3\n  v.take(n).sum()"), 6);
    assert_eq!(answer("var n = 2\n  v.skip(n).sum()"), 12);
    // An arithmetic expression, and one that calls back into the receiver.
    assert_eq!(answer("var n = 1\n  v.take(n + n).sum()"), 3);
    assert_eq!(answer("v.skip(v.len() - 2).sum()"), 9);
    // The bound still composes with the stages around it.
    assert_eq!(answer("var n = 4\n  v.take(n).map(|x| x * 10).sum()"), 100);
    assert_eq!(answer("var n = 4\n  v.take(n).filter(|x| x > 2).sum()"), 7);
    // Degenerate bounds: `take` of nothing is empty, `skip` of nothing drops
    // nothing, and a negative bound is the same comparison rather than a special
    // case.
    assert_eq!(answer("var n = 0\n  v.take(n).sum()"), 0);
    assert_eq!(answer("var n = 0\n  v.skip(n).sum()"), 15);
    assert_eq!(answer("var n = 0 - 1\n  v.take(n).sum()"), 0);
    assert_eq!(answer("var n = 0 - 1\n  v.skip(n).sum()"), 15);
    assert_eq!(answer("var n = 99\n  v.take(n).sum()"), 15);
    assert_eq!(answer("var n = 99\n  v.skip(n).sum()"), 0);
}

#[test]
fn pipeline_take_while_stops_at_predicate() {
    // [1,2,3,4,1].take_while(<4) = [1,2,3] (stops at first 4, does NOT resume
    // at the trailing 1). sum() = 6.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.push(1)\n  v.take_while(|x| x < 4).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 6);
}

#[test]
fn pipeline_take_while_then_count() {
    // [2,4,6,1,8].take_while(even) = [2,4,6].count() = 3.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(2)\n  v.push(4)\n  v.push(6)\n  v.push(1)\n  v.push(8)\n  v.take_while(|x| x - x / 2 * 2 == 0).count()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

#[test]
fn pipeline_enumerate_count() {
    // enumerate produces (i, item) pairs; here we only count them → 3.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(10)\n  v.push(20)\n  v.push(30)\n  v.enumerate().count()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

/// `enumerate` numbers the sequence that reaches it. After a `filter` that is a
/// dense 0, 1, 2 …, not the surviving source positions.
#[test]
fn enumerate_after_filter_numbers_the_filtered_sequence() {
    // [1,2,3,4] -filter(even)-> [2,4] -enumerate-> (0,2), (1,4).
    // Weighted 100*index + value: 2 + 104 = 106. Reading source indices would
    // give (1,2), (3,4) → 406, and a swap of the halves gives something else
    // again.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  var t = 0\n  for p in v.filter(|x| x % 2 == 0).enumerate() { t = t + p.0 * 100 + p.1 }\n  t\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 106);

    // And after a `skip`, which drops from the front: [1,2,3,4].skip(2) is
    // [3,4], numbered (0,3), (1,4) → 3 + 104 = 107.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  var t = 0\n  for p in v.skip(2).enumerate() { t = t + p.0 * 100 + p.1 }\n  t\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 107);
}

/// Every stage that asks "which element is this?" is asking about *its own*
/// input sequence — the one that reaches it — not about the source.
///
/// One shared counter answers all the single-stage cases correctly, so the
/// shapes that force the general rule are these: two position-consuming stages
/// with a `filter` between them, where one counter and two counters disagree.
#[test]
fn each_stage_counts_the_sequence_that_reaches_it() {
    // [1..6].skip(1) = [2,3,4,5,6]; filter(even) = [2,4,6]; take(2) = [2,4].
    // Sum 6. With one source cursor, `take` stops once the *source* index
    // reaches 2, so only the 2 survives and the answer is 2.
    let six = "  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.push(5)\n  v.push(6)\n";
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
            "var rhs = Vec()\n  rhs.push(10)\n  rhs.push(20)\n  rhs.push(30)\n  v.filter(|x| x % 2 == 0).zip(rhs).take(2).count()"
        ),
        2
    );

    // `position` reports the position in the sequence that reached the sink, and
    // it must not be overwritten by a later match — which is what happens when a
    // hit inside a `flat_map` ends only the inner loop. The inner Vecs here are
    // [0, 5] and [10]: flattened, the first element over 4 is at index 1;
    // per-inner, the first Vec answers 1 and the second overwrites it with 0.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  match v.flat_map(|x| {\n    var r = Vec()\n    if x == 1 { r.push(0) }\n    r.push(x * 5)\n    r\n  }).position(|p| p > 4) { Some(i) => i, None => -1 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1, "the flattened stream's index, once");
}

#[test]
fn pipeline_zip_count_pairs_to_shorter() {
    // [1,2,3].zip([10,20]) = 2 pairs (shorter length). count() = 2.
    let src = "fn main() -> Int {\n  var a = Vec()\n  a.push(1)\n  a.push(2)\n  a.push(3)\n  var b = Vec()\n  b.push(10)\n  b.push(20)\n  a.zip(b).count()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn pipeline_flat_map_collect_len() {
    // [1,2,3].flat_map(|x| Vec-of-two) → 6 elements. Each closure returns a
    // 2-element Vec via push. We then collect and read len.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  var out = v.flat_map(|x| {\n    var r = Vec()\n    r.push(x)\n    r.push(x)\n    r\n  })\n  out.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 6);
}

#[test]
fn pipeline_flat_map_sum() {
    // [1,2,3].flat_map(|x| Vec(x, x*10)) = [1,10,2,20,3,30].sum() = 66.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.flat_map(|x| {\n    var r = Vec()\n    r.push(x)\n    r.push(x * 10)\n    r\n  }).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 66);
}

/// A `flat_map` inside a `flat_map` flattens *both* levels, in order, with
/// everything between them applied once per element of the level it sits in.
///
/// `two_flat_map_stages_compose_without_a_compiler_panic` asserts only that the
/// compiler survives, plus a count — which a wrong-but-non-panicking nesting
/// could also produce. These weight every level so that dropping one, running
/// one at the wrong depth, or ordering the two backwards all answer a different
/// number.
#[test]
fn a_flat_map_inside_a_flat_map_flattens_both_levels() {
    // [1,2] -flat_map(x -> [x, x*10])-> [1,10,2,20]
    //       -flat_map(y -> [y, y*100])-> [1,100,10,1000,2,200,20,2000]
    // sum = 101 * (1 + 10 + 2 + 20) = 3333, and there are eight elements.
    let outer = "  var v = Vec()\n  v.push(1)\n  v.push(2)\n";
    let two_levels = "v.flat_map(|x| {\n    var a = Vec()\n    a.push(x)\n    a.push(x * 10)\n    a\n  }).flat_map(|y| {\n    var c = Vec()\n    c.push(y)\n    c.push(y * 100)\n    c\n  })";
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
        "fn main() -> Int {{\n{outer}  v.flat_map(|x| {{\n    var a = Vec()\n    a.push(x)\n    a.push(x * 10)\n    a\n  }}).map(|y| y * 2).flat_map(|z| {{\n    var c = Vec()\n    c.push(z)\n    c.push(z + 1)\n    c\n  }}).sum()\n}}\n"
    ));
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(
        result.as_int(),
        136,
        "a stage between two splices runs at the depth it was written at"
    );
}

/// A stage that stops the stream stops the *stream*, not the inner Vec it
/// happened to be looking at.
///
/// Applied per inner Vec instead, `take_while` silently becomes a `filter`, so
/// elements after the stop point are processed and can fault.
#[test]
fn take_while_after_flat_map_stops_the_whole_stream() {
    // [3,1,5] -flat_map(x -> [x])-> [3,1,5] -take_while(> 2)-> [3], and
    // 100 / (3 - 5) = -50. Per inner Vec, `1` is merely dropped and `5` goes on
    // to divide by zero — which is the assertion, because a wrong answer here
    // would be indistinguishable from a right one for a total mapper.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(3)\n  v.push(1)\n  v.push(5)\n  v.flat_map(|x| {\n    var a = Vec()\n    a.push(x)\n    a\n  }).take_while(|y| y > 2).map(|y| 100 / (y - 5)).sum()\n}\n";
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
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.flat_map(|x| {\n    var a = Vec()\n    a.push(x)\n    a.push(x * 10)\n    a\n  }).take_while(|y| y < 5).count()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1, "the stream stops at the first 10");
}

/// `filter_map`'s closure is `(Int) -> Option[U]`, and the `None`s it answers
/// must not survive into the sink.
///
/// Asserting a *sum* is what makes that measurable in one integer: surviving
/// `None`s would not merely change the number, they would fail to type-check at
/// `sum` (`error[Y001]: expected Int, found Option[Int]` — the user is told
/// their sum is wrong and never that `filter_map` did not filter).
#[test]
fn filter_map_drops_the_nones_rather_than_carrying_them() {
    // [1,2,3,4,5] |> Some(x*2) when x > 2 → [6, 8, 10], summing to 24.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.push(5)\n  v.filter_map(|x| if x > 2 { Some(x * 2) } else { None }).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 24);
}

/// The all-`None` end of the same range: a `filter_map` that keeps nothing
/// answers an empty sequence, not a sequence of `None`s. `count()` is the
/// measurement because it is the one sink that a surviving `None` would inflate
/// without any type error to hide behind.
#[test]
fn a_filter_map_that_matches_nothing_yields_nothing() {
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.filter_map(|x| if x > 100 { Some(x) } else { None }).len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

/// `filter_map` composes with the stages either side of it — a `None` leaves by
/// the *innermost* continue edge, and does not end the chain.
#[test]
fn filter_map_in_the_middle_of_a_chain_drops_only_its_own_element() {
    // [1..6] .map(+1) = [2,3,4,5,6]
    //        .filter_map(Some(x*10) when even) = [20, 40, 60]
    //        .filter(> 20) = [40, 60] → 100
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.push(5)\n  v.map(|x| x + 1).filter_map(|x| if x - x / 2 * 2 == 0 { Some(x * 10) } else { None }).filter(|x| x > 20).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 100);
}

#[test]
fn pipeline_min_by_with_comparator() {
    // min_by picks the element for which the comparator (a < b) holds vs. the
    // running best. [|10, 5, 8|].min_by(|a, b| a < b) = 5.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(10)\n  v.push(5)\n  v.push(8)\n  v.min_by(|a, b| a < b)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 5);
}

#[test]
fn pipeline_max_by_with_comparator() {
    // max_by picks the element for which the comparator says candidate < best
    // (i.e. best is "less than" candidate → candidate is larger).
    // [|3, 7, 2|].max_by(|a, b| a < b) = 7.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(3)\n  v.push(7)\n  v.push(2)\n  v.max_by(|a, b| a < b)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 7);
}

#[test]
fn pipeline_empty_vec_sum_is_zero() {
    // An empty source: the fused loop body never runs; sum accumulator stays 0.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

/// An empty source is a miss like any other, and a miss is `None`. Note this is
/// *not* the `EmptyCollection` fault: `find` has an answer for an empty
/// sequence, which is exactly what distinguishes it from `min`/`reduce`.
#[test]
fn an_empty_source_makes_find_answer_none() {
    let src = "fn main() -> Int {\n  var v = Vec()\n  match v.find(|x| x == 0) { Some(n) => n, None => 7 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 7);
}

// ===========================================================================
// Text methods (§4.3) and `out(...)` (§16.1).
// ===========================================================================

#[test]
fn text_len_and_get_end_to_end() {
    // Text literals allocate; .len() counts chars; .get(i) answers the `Char`
    // there, whose scalar value is named with `.to_int()` (ADR-086).
    let src = "fn main() -> Int {\n  var s = \"hello\"\n  s.get(1).to_int()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault());
    // 'e' = 101
    assert_eq!(result.as_int(), 101);
}

#[test]
fn text_len_counts_unicode_scalars() {
    // "héllo" has 5 Unicode scalar values (é is one char).
    let src = "fn main() -> Int {\n  var s = \"héllo\"\n  s.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 5);
}

#[test]
fn text_get_indexes_by_scalar_not_byte() {
    // `praxis_text_get` must index by Unicode scalar value, not by byte: in
    // "héllo" the char at index 1 is é (scalar 233), but é is encoded as two
    // bytes (0xC3 0xA9), so byte indexing would return 0xC3 (195) instead. Input
    // parsing produces Text values that get indexed into, so the distinction is
    // load-bearing.
    let src = "fn main() -> Int {\n  var s = \"héllo\"\n  s.get(1).to_int()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 233);
}

/// **ADR-086 end to end.** The two halves — the catalog row's type and the
/// runtime's descriptor — must agree, and this is the test that proves it.
///
/// Each case is red for its own reason, which is the point of having three:
/// - (a) and (b) are `Y110 no method` without the conversion rows.
/// - (b) and (c) *abort* with the runtime half reverted and the catalog half in
///   place: a `Char`-typed comparison lowers through `praxis_char_load`, whose
///   `read_scalar` answers `None` against the `INT` descriptor. A half-fix is
///   loud, not silent.
/// - (c) is `Y001 expected Char, found Int` with the catalog half reverted and
///   the runtime half in place.
#[test]
fn a_char_and_its_code_point_convert_both_ways() {
    // (a) Non-ASCII on purpose: this also pins scalar-not-byte indexing through
    // the `Char` type.
    let (rt, result) = run_main("fn main() -> Int {\n  \"héllo\"[1].to_int()\n}\n");
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 233);

    // (b) The round trip. `Int.to_char()` is the half that makes `Vec[Char]`
    // and `Map[Char, _]` writable from the language rather than read-only.
    let src =
        "fn main() -> Int {\n  var c = \"héllo\"[1]\n  if 233.to_char() == c { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 1);

    // (c) `"#"[0]` is how a program names a particular character while there is
    // no char literal, so a `Grid[Char]` cell is comparable to one.
    let src = "fn main() -> Int {\n  var g = read grid(char)\n  if g[1, 0] == \"#\"[0] { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main_with_input(src, "a#\ncd\n");
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 1);
}

/// `Int.to_char()` is the narrowing half, so it faults where its partner
/// cannot — `Float.to_int()`'s relationship to `Int.to_float()` exactly.
#[test]
fn int_to_char_faults_on_a_value_that_is_not_a_scalar() {
    let (rt, _) = run_main("fn main() -> Char {\n  55296.to_char()\n}\n");
    assert!(rt.has_pending_fault());
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::InvalidChar);
}

#[test]
fn text_is_empty_works() {
    // An empty text literal's .is_empty() → Bool → compare as 1.
    let src = "fn main() -> Int {\n  var s = \"\"\n  if s.is_empty() { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn text_get_out_of_bounds_faults() {
    let src = "fn main() -> Int {\n  var s = \"ab\"\n  s.get(5)\n}\n";
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
// Reassignment and object mutation through a binding, under GC (§4.2).
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
    // §4.2: "A binding may still point to a mutable object." Push 1000
    // elements to a `var v` (mutating the object), survive GCs, read back.
    let src = "fn main() -> Int {\n  var v = Vec()\n  var i = 0\n  while i < 1000 { v.push(i * 2); i = i + 1 }\n  v.get(500)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 1000); // 500 * 2
}

// ===========================================================================
// Char wired end-to-end (§4.3): inference → HIR → MIR → codegen → runtime. The
// input parser produces Char values (`char` atom, `grid(char)`); these tests
// exercise the runtime allocation path it uses.
// ===========================================================================

#[test]
fn char_type_annotation_is_accepted() {
    // `Char` must type-check. This compiles through the whole pipeline
    // (resolve → infer → lower → MIR → JIT) without error.
    let src = "fn main() -> Char {\n  out(0)\n}\n";
    let (_jit, ids) = compile(src);
    assert!(ids.contains_key("main"), "Char return type compiles");
}

#[test]
fn char_runtime_roundtrip() {
    // The descriptor + allocator path the input parser calls: alloc_char
    // stores a u32 scalar; as_char recovers it. Exercises scalars::CHAR.
    let rt = Runtime::new();
    let c = rt.alloc_char('€' as u32);
    assert_eq!(c.as_char(), '€');
    // A simple ASCII char.
    let a = rt.alloc_char('A' as u32);
    assert_eq!(a.as_char(), 'A');
}

// ===========================================================================
// The `read` input parser (§7), end to end: source → parse → infer → lower →
// MIR → JIT → run the parser plan.
// ===========================================================================

#[test]
fn read_lines_of_int_parses_input() {
    // `read lines(int)` against "10\n20\n30" → Vec[Int] of [10, 20, 30].
    // The program reads .len() and returns it.
    let src = "fn main() -> Int {\n  var v = read lines(int)\n  v.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "10\n20\n30\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

#[test]
fn read_lines_of_int_first_element() {
    // Read the first element of a lines(int) parse.
    let src = "fn main() -> Int {\n  var v = read lines(int)\n  v.get(0)\n}\n";
    let (rt, result) = run_main_with_input(src, "42\n99\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

#[test]
fn read_with_var_binding() {
    // Bind read results with `var`.
    let src = "fn main() -> Int {\n  var v = read lines(int)\n  v.get(1)\n}\n";
    let (rt, result) = run_main_with_input(src, "10\n20\n30\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 20);
}

#[test]
fn multiple_reads_parse_same_buffer() {
    // Multiple `read` expressions deterministically parse the same complete
    // source buffer.
    let src = "fn main() -> Int {\n  var a = read lines(int)\n  var b = read lines(int)\n  a.get(0) + b.get(1)\n}\n";
    let (rt, result) = run_main_with_input(src, "100\n200\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 300); // 100 + 200
}

#[test]
fn read_sections_lines_csv_int_nested() {
    // `read sections(lines(csv(int)))` — the §7.6 nested example, producing
    // Vec[Vec[Vec[Int]]]. The assertion is the outer structure: non-faulting,
    // and a Vec of the right length.
    let src =
        "fn main() -> Int {\n  var groups = read sections(lines(csv(int)))\n  groups.len()\n}\n";
    let input = "1,2,3\n4,5,6\n\n7,8,9\n";
    let (rt, result) = run_main_with_input(src, input);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2); // two sections
}

// --- Input-parser templates and nested descriptors (§7.2) ------------------

#[test]
fn read_lines_of_named_capture_template_parses_records() {
    // `read lines(`{x:int},{y:int}`)` → Vec[{x:Int,y:Int}]. Each line matches the
    // template; named captures become record fields. We read .len() to confirm
    // three records parsed without faulting.
    let src = "fn main() -> Int {\n  var v = read lines(`{x:int},{y:int}`)\n  v.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "1,2\n3,4\n5,6\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

#[test]
fn read_lines_of_single_anon_capture_parses_scalars() {
    // `read lines(`{int}`)` → Vec[Int]. A single anonymous capture yields the
    // scalar value directly. Read the first element to confirm the value flows.
    let src = "fn main() -> Int {\n  var v = read lines(`{int}`)\n  v.get(1)\n}\n";
    let (rt, result) = run_main_with_input(src, "10\n20\n30\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 20);
}

#[test]
fn read_lines_of_multi_anon_capture_parses_tuples() {
    // `read lines(`{int},{int}`)` → Vec[(Int, Int)]. Two anonymous captures
    // assemble into a tuple. We read .len() to confirm parsing succeeded.
    let src = "fn main() -> Int {\n  var v = read lines(`{int},{int}`)\n  v.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "1,2\n3,4\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn read_standalone_named_capture_template_parses_one_record() {
    // A standalone template (no `lines`) against the whole buffer. `{x:int},{y:int}`
    // parses a single record from "7,8".
    let src = "fn main() -> Int {\n  var r = read `{x:int},{y:int}`\n  0\n}\n";
    let (rt, result) = run_main_with_input(src, "7,8\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

#[test]
fn read_nested_collections_descriptor_is_composite() {
    // `read sections(lines(csv(int)))` tags the outer Vec's element descriptor
    // as a Vec (not the leaf Int), so formatting/equality on nested
    // collections dispatches correctly. Compare two identical nested parses for
    // structural equality → true (1).
    let src = "fn main() -> Int {\n  var a = read sections(lines(csv(int)))\n  var b = read sections(lines(csv(int)))\n  if a == b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main_with_input(src, "1,2\n3,4\n\n1,2\n3,4\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_parser_record_with_text_field_equal_to_literal_record() {
    // Parser-built records take each field's descriptor from the value
    // (`alloc_record` uses `value.descriptor()`), because record_equals/format/
    // hash dispatch through the SCHEMA's field descriptor (records.rs): an INT
    // descriptor over a Text field would reinterpret a TextPayload as i64.
    // Two identical parses → equal (1).
    let src = "fn main() -> Int {\n  var a = read lines(`{name:word},{port:int}`)\n  var b = read lines(`{name:word},{port:int}`)\n  if a == b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main_with_input(src, "alpha,80\nbeta,443\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_parser_record_with_text_field_unequal_when_differs() {
    // Complement: two parser records whose Text fields differ must compare
    // unequal (no false-positive pointer collision).
    let src = "fn main() -> Int {\n  var a = read lines(`{name:word},{port:int}`)\n  var b = read lines(`{name:word},{port:int}`)\n  if a == b { 1 } else { 0 }\n}\n";
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
    // dispatch through the field descriptor — an INT descriptor would
    // reinterpret a TextPayload. Insert the same parser record twice; the set
    // must dedupe to 1.
    let src = "fn main() -> Int {\n  var recs = read lines(`{name:word},{port:int}`)\n  var s = Set()\n  s.insert(recs.get(0))\n  s.insert(recs.get(0))\n  s.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "alpha,80\nbeta,443\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_parser_record_with_text_field_survives_gc() {
    // Parser records with Text fields must survive GC (record_trace traces
    // items directly, so this should work). Force GC then read len.
    // Template mirrors the working {x:int},{y:int} pattern but with a word field.
    let src = "fn main() -> Int {\n  var recs = read lines(`{name:word},{port:int}`)\n  var garbage = Vec()\n  var i = 0\n  while i < 500 { garbage.push(i); i = i + 1 }\n  recs.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "alpha,80\nbeta,443\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

// --- Adversarial: parser-record schema cache (§6.1) -------------------------
//
// `record_schema_for` keys its cache on the (field-name, descriptor) sequence,
// not on the names alone: two templates with identical field names but different
// capture types (e.g. `{x:word}` vs `{x:int}`) must not share a schema.
// record_equals/format/hash dispatch through `schema.fields[i].descriptor`, so a
// name-only cache would compare/format the second template's fields through the
// wrong callback. `tuple_schema_for` is descriptor-keyed for the same reason.

#[test]
fn adv_parser_record_same_name_diff_type_no_schema_collision() {
    // Two record templates with the SAME field name `v` but DIFFERENT capture
    // types (`word` → Text vs `int` → Int) must not share a schema. We parse
    // each into a Vec, then compare a record to itself (forces record_equals
    // through the schema descriptor) and use it as a Set key (forces
    // record_hash). Both must succeed without faulting; a shared schema would
    // reinterpret a TextPayload through `INT.equals`.
    let src = String::from("fn main() -> Int {\n")
        // First template seen: {v:word} → v is Text. Parse, compare, key it.
        + "  var ws = read lines(`{v:word}`)\n"
        + "  var w_ok = if ws.get(0) == ws.get(0) { 1 } else { 0 }\n"
        + "  var ws_set = Set()\n"
        + "  ws_set.insert(ws.get(0))\n"
        // Second template seen: {v:char} → v is Char, SAME field name `v`.
        // Both parse the single char `a`, but the field descriptor differs
        // (TEXT vs CHAR). A name-only cache would hand this the word template's
        // Text descriptor, miscomparing/mishashing the Char field.
        + "  var cs = read lines(`{v:char}`)\n"
        + "  var c_ok = if cs.get(0) == cs.get(0) { 1 } else { 0 }\n"
        + "  var cs_set = Set()\n"
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
        + "  var ws = read lines(`{v:word}`)\n"
        + "  var garbage = Vec()\n"
        + "  var i = 0\n"
        + "  while i < 300 { garbage.push(i); i = i + 1 }\n"
        + "  var cs = read lines(`{v:char}`)\n"
        + "  ws.len() + cs.len()\n"
        + "}\n";
    let (rt, result) = run_main_with_input(&src, "a\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn adv_read_against_non_text_input_faults_cleanly() {
    // §6.3: `read` against a non-Text input_source must fault cleanly, because
    // run_plan would otherwise reinterpret input.payload as a TextPayload and
    // deref a garbage pointer. `praxis_get_input` checks the descriptor and, for
    // a non-Text source, raises ParseFailed and returns the Unit sentinel
    // instead of handing the parser garbage. This program reads against the
    // UNSET (Unit) input_source, so it must return with a clean ParseFailed
    // fault and the host must stay alive.
    let src = "fn main() -> Int {\n  var v = read lines(`{x:word}`)\n  v.len()\n}\n";
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
    // A CSV inside a section starts at a non-zero byte offset (parser.rs
    // walk_csv): each field's offset must be computed from the section's base
    // rather than recovered by re-slicing the section and walking its child at
    // offset 0.
    //
    // `csv` describes one line; a section of several lines is `lines(csv(...))`.
    // Under §7.5's full-consumption rule a `csv` region containing a newline is
    // a parse failure.
    //
    // **What this test can and cannot distinguish.** The `Int` assertion below
    // is not a differential and is not claimed as one: a misresolved duplicate
    // field still parses to the same number. The two halves that *are*
    // observable follow it: a `Text` read out of the *second* section, and a
    // field that trims to nothing.
    //
    // §7.5's full-consumption half is carried by
    // `a_csv_field_the_child_does_not_consume_is_a_parse_failure` and
    // `csv_rest_parser_is_bounded_to_each_token` (parser.rs), not by this test.
    let src = "fn main() -> Int {\n  var s = read sections(lines(csv(int)))\n  \
               s.get(1).get(0).get(1)\n}\n";
    let (rt, result) = run_main_with_input(src, "1,2,3\n4,5,6\n\n7,8\n9,10\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(
        result.as_int(),
        8,
        "the second section's first line's second field is 8, at byte 15 of the input"
    );

    // A `Text` field out of the second section. Every field text is distinct,
    // so any offset that names the wrong field — a re-based-to-zero section
    // walk, or a search that resolves inside the first section — answers with
    // different bytes and this fails.
    let src = "fn main() -> Text {\n  var s = read sections(lines(csv(word)))\n  \
               s.get(1).get(1).get(0)\n}\n";
    let (rt, result) = run_main_with_input(src, "aa,bb\ncc,dd\n\nee,ff\ngg,hh\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(
        result.as_text(),
        "gg",
        "the second section's second line's first field, at byte 19 — not `aa`"
    );

    // A field that trims to nothing. The assertion is the empty field's own
    // length: a panic here is not a failed assertion, it is undefined behaviour
    // across the ABI.
    let src = "fn main() -> Int {\n  var s = read sections(lines(csv(rest)))\n  \
               s.get(1).get(0).get(2).len()\n}\n";
    let (rt, result) = run_main_with_input(src, "1,2,3\n\n10,20,\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0, "a field that trims to nothing is empty");
}

/// A `csv` field whose region contains anything the child parser does not
/// consume is a parse failure, not a silent truncation: the child is walked
/// against the field's bounds, not against everything to the end of the buffer.
#[test]
fn a_csv_field_the_child_does_not_consume_is_a_parse_failure() {
    let (rt, _result) = run_main_with_input(
        "fn main() -> Int {\n  var v = read csv(int)\n  v.len()\n}\n",
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
    let src = "fn main() -> Int {\n  var v = read csv(int)\n  v.get(3)\n}\n";
    let (rt, result) = run_main_with_input(src, "10,20,30,40,50\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 40);
}

#[test]
fn adv_read_empty_input_yields_empty_vec() {
    // Empty input to `read lines(int)`: should yield an empty Vec, not fault.
    let src = "fn main() -> Int {\n  var v = read lines(int)\n  v.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

#[test]
fn adv_grid_rotate_four_times_is_identity() {
    // Rotate right 4× → original. Verifies the rotate operation composes
    // correctly (a single rotate-is-some-permutation is already tested).
    let src = "fn main() -> Int {\n  var g = read grid(char)\n  var r1 = g.rotate_right()\n  var r2 = r1.rotate_right()\n  var r3 = r2.rotate_right()\n  var r4 = r3.rotate_right()\n  if g == r4 { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main_with_input(src, "abc\ndef\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_grid_transpose_twice_is_identity() {
    // Transpose is its own inverse for a rectangular grid.
    let src = "fn main() -> Int {\n  var g = read grid(char)\n  var t1 = g.transpose()\n  var t2 = t1.transpose()\n  if g == t2 { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main_with_input(src, "abc\ndef\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_grid_equality_false_for_different_content() {
    // Two grids of the same dimensions but different content must compare
    // unequal (guards against a width-only equality shortcut).
    let src = "fn main() -> Int {\n  var g = read grid(char)\n  var h = read grid(char)\n  if g == h { 1 } else { 0 }\n}\n";
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
    let src = "fn main() -> Int {\n  var g = read grid(char)\n  var garbage = Vec()\n  var i = 0\n  while i < 500 { garbage.push(i); i = i + 1 }\n  g.width()\n}\n";
    let (rt, result) = run_main_with_input(src, &input);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 30);
}

#[test]
fn adv_bitset_remove_high_bit_then_equals_untouched() {
    // Removing a high bit leaves a trailing zero word (bitset.rs); equals/hash
    // must still treat it as distinct from a never-touched bitset of the same
    // low bits. Guards the equals⇒hash-equal invariant for the
    // trailing-zero-word case.
    let src = "fn main() -> Int {\n  var a = BitSet()\n  a.insert(100)\n  a.remove(100)\n  var b = BitSet()\n  var ea = a.contains(1)\n  var eb = b.contains(1)\n  if ea == eb { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // both empty after a's remove → neither contains 1 → ea==eb (both false)
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_bitset_large_under_gc_pressure() {
    // Insert 500 bits under GC; the bitset backing must survive.
    let src = "fn main() -> Int {\n  var b = BitSet()\n  var i = 0\n  while i < 500 { b.insert(i); i = i + 1 }\n  var garbage = Vec()\n  var j = 0\n  while j < 500 { garbage.push(j); j = j + 1 }\n  var p = b.contains(499)\n  if p { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_min_heap_ordering_under_gc_pressure() {
    // Push 200 ints to a MinHeap under GC, pop all and confirm ascending order
    // by checking the first pop is the min.
    let src = "fn main() -> Int {\n  var h = MinHeap()\n  var i = 0\n  while i < 200 { h.push((i * 37 + 11) - 100); i = i + 1 }\n  var garbage = Vec()\n  var j = 0\n  while j < 300 { garbage.push(j); j = j + 1 }\n  h.pop()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // (i*37+11)-100 increases with i, so the minimum is at i=0: -89.
    assert_eq!(result.as_int(), -89);
}

#[test]
fn adv_tuple_with_record_field_equality() {
    // A tuple containing records — equality must dispatch through each
    // element's own descriptor (tuples.rs uses item.descriptor()). Two
    // structurally-equal tuples must compare equal.
    let src = "struct P { x: Int, y: Int }\nfn main() -> Int {\n  var a = (P { x: 1, y: 2 }, P { x: 3, y: 4 })\n  var b = (P { x: 1, y: 2 }, P { x: 3, y: 4 })\n  if a == b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_nested_vec_equality_deep() {
    // Deeply nested Vec equality: Vec[Vec[Vec[Int]]]. Equal shapes+content.
    let src = "fn main() -> Int {\n  var a = Vec()\n  var inner_a = Vec()\n  var leaf_a = Vec()\n  leaf_a.push(1)\n  leaf_a.push(2)\n  inner_a.push(leaf_a)\n  a.push(inner_a)\n  var b = Vec()\n  var inner_b = Vec()\n  var leaf_b = Vec()\n  leaf_b.push(1)\n  leaf_b.push(2)\n  inner_b.push(leaf_b)\n  b.push(inner_b)\n  if a == b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_nested_vec_equality_unequal_leaf() {
    // Complement: differing leaf content → unequal.
    let src = "fn main() -> Int {\n  var a = Vec()\n  var inner_a = Vec()\n  inner_a.push(1)\n  inner_a.push(2)\n  a.push(inner_a)\n  var b = Vec()\n  var inner_b = Vec()\n  inner_b.push(1)\n  inner_b.push(9)\n  b.push(inner_b)\n  if a == b { 1 } else { 0 }\n}\n";
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
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.get(5)\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "OOB vec get should fault");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::IndexOutOfBounds);
}

#[test]
fn adv_out_of_bounds_vec_negative_index_faults() {
    // Negative index — must fault cleanly, not wrap to a huge usize and crash.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.get(0 - 1)\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "negative index should fault");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::IndexOutOfBounds);
}

// --- short-circuit || and ! -------------------------------------------------

#[test]
fn logical_or_returns_true_when_lhs_true() {
    // true || false → true (→ 1).
    let src = "fn main() -> Int {\n  var b = true || false\n  if b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn logical_or_returns_rhs_when_lhs_false() {
    // false || true → true (→ 1).
    let src = "fn main() -> Int {\n  var b = false || true\n  if b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn logical_or_returns_false_when_both_false() {
    // false || false → false (→ 0).
    let src = "fn main() -> Int {\n  var b = false || false\n  if b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

#[test]
fn logical_or_short_circuits_skipping_rhs_side_effect() {
    // Short-circuit: when lhs is true, the rhs division by zero must NOT
    // execute (no fault). If || were eager, this would fault.
    let src = "fn main() -> Int {\n  var b = true || (1 / 0 == 0)\n  if b { 1 } else { 0 }\n}\n";
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
    let src = "fn main() -> Int {\n  var b = false || (1 / 0 == 0)\n  if b { 1 } else { 0 }\n}\n";
    let (rt, _result) = run_main(src);
    assert!(
        rt.has_pending_fault(),
        "rhs must be evaluated when lhs is false"
    );
}

#[test]
fn logical_not_flips_bool() {
    // !true → false (→ 0); !false → true (→ 1).
    let src = "fn main() -> Int {\n  var a = !true\n  var b = !false\n  if a { 0 } else { if b { 1 } else { 0 } }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn double_not_is_identity() {
    // !!true → true (→ 1).
    let src = "fn main() -> Int {\n  var b = !!true\n  if b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

// --- nominal records (§4.5) -------------------------------------------------

#[test]
fn record_construction_and_field_access() {
    // `Point { x: 3, y: 4 }` → read back x + y = 7.
    let src = "struct Point { x: Int, y: Int }\nfn main() -> Int {\n  var p = Point { x: 3, y: 4 }\n  p.x + p.y\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 7);
}

#[test]
fn record_field_access_independently() {
    // Read just one field.
    let src = "struct Point { x: Int, y: Int }\nfn main() -> Int {\n  var p = Point { x: 30, y: 4 }\n  p.x\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 30);
}

#[test]
fn record_with_text_field() {
    // A record with a Text field, accessed and used.
    let src = "struct Entry { key: Int, label: Text }\nfn main() -> Int {\n  var e = Entry { key: 42, label: \"hello\" }\n  e.key\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

#[test]
fn record_survives_gc() {
    // Allocate a record, trigger GC by allocating many objects, then read back.
    // This verifies the record's GcRef is rooted across safepoints.
    let src = "struct Point { x: Int, y: Int }\nfn main() -> Int {\n  var p = Point { x: 100, y: 200 }\n  var i = 0\n  while i < 100 {\n    var junk = i + 1\n    i = i + 1\n  }\n  p.x + p.y\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 300);
}

#[test]
fn record_field_punning() {
    // Field punning: `Point { x, y }` where x and y are bindings.
    let src = "struct Point { x: Int, y: Int }\nfn main() -> Int {\n  var x = 5\n  var y = 7\n  var p = Point { x, y }\n  p.x * p.y\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 35);
}

// --- tuples (§4.5 structural tuples) ----------------------------------------

#[test]
fn tuple_construction_does_not_fault() {
    // Tuples materialize as real objects; constructing one must not fault.
    let src = "fn main() -> Int {\n  var t = (1, 2, 3)\n  7\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 7);
}

#[test]
fn tuple_survives_gc() {
    // Allocate a tuple, trigger GC by allocating many objects, and confirm the
    // program completes without faulting (the tuple's GcRef must be rooted
    // across safepoints).
    let src = "fn main() -> Int {\n  var t = (100, 200)\n  var i = 0\n  while i < 100 {\n    var junk = i + 1\n    i = i + 1\n  }\n  9\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 9);
}

#[test]
fn tuple_of_mixed_types() {
    // A tuple with heterogeneous element types (Int, Bool) constructs cleanly.
    let src = "fn main() -> Int {\n  var t = (42, true)\n  5\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 5);
}

// --- structural equality (§5.5) ---------------------------------------------

#[test]
fn record_equality_true() {
    // `Point{1,2} == Point{1,2}` → true (1). Bind to `var` first because record
    // literals are blocked in `if` conditions (`parse_expr_no_struct_lit`).
    let src = "struct Point { x: Int, y: Int }\nfn main() -> Int {\n  var a = Point { x: 1, y: 2 }\n  var b = Point { x: 1, y: 2 }\n  if a == b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn record_equality_false() {
    // `Point{1,2} == Point{1,3}` → false (0).
    let src = "struct Point { x: Int, y: Int }\nfn main() -> Int {\n  var a = Point { x: 1, y: 2 }\n  var b = Point { x: 1, y: 3 }\n  if a == b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

#[test]
fn record_inequality() {
    // `Point{1,2} != Point{1,3}` → true (1).
    let src = "struct Point { x: Int, y: Int }\nfn main() -> Int {\n  var a = Point { x: 1, y: 2 }\n  var b = Point { x: 1, y: 3 }\n  if a != b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn tuple_equality_true() {
    // `(1, 2) == (1, 2)` → true.
    let src =
        "fn main() -> Int {\n  var a = (1, 2)\n  var b = (1, 2)\n  if a == b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn tuple_equality_false() {
    // `(1, 2) == (1, 3)` → false.
    let src =
        "fn main() -> Int {\n  var a = (1, 2)\n  var b = (1, 3)\n  if a == b { 1 } else { 0 }\n}\n";
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
    let src = "enum Tile { Empty, Number(Int) }\nfn main() -> Int {\n  var a = Number(42) == Number(42)\n  var b = Number(42) == Number(7)\n  if a { if b { 3 } else { 2 } } else { 1 }\n}\n";
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

// --- enums (§4.6) -----------------------------------------------------------

#[test]
fn enum_payload_variant_construction() {
    // `Number(5)` constructs a payload variant. Verify construction doesn't fault.
    let src =
        "enum Tile { Empty, Number(Int) }\nfn main() -> Int {\n  var t = Number(42)\n  7\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 7);
}

#[test]
fn enum_zero_payload_variant_as_value() {
    // `Empty` is a bare zero-payload variant value.
    let src = "enum Tile { Empty, Number(Int) }\nfn main() -> Int {\n  var t = Empty\n  8\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 8);
}

#[test]
fn enum_construction_does_not_fault() {
    // Direct enum construction — verify it allocates without faulting.
    let src = "enum Tile { Empty, Wall, Number(Int) }\nfn main() -> Int {\n  var a = Empty\n  var b = Wall\n  var c = Number(99)\n  42\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

#[test]
fn enum_survives_gc() {
    // Allocate enum values, trigger GC, verify no fault.
    let src = "enum Tile { Empty, Number(Int) }\nfn main() -> Int {\n  var t = Number(123)\n  var i = 0\n  while i < 50 {\n    var junk = i + 1\n    i = i + 1\n  }\n  456\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 456);
}

// --- pattern matching (§4.6) ------------------------------------------------

#[test]
fn match_enum_wall_arm() {
    let src = "enum Tile { Empty, Wall }\nfn main() -> Int {\n  var t = Wall\n  match t {\n    Empty => 1\n    Wall => 2\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn match_enum_with_wildcard_default() {
    let src = "enum Tile { Empty, Wall, Number(Int) }\nfn main() -> Int {\n  var t = Wall\n  match t {\n    Empty => 1\n    _ => 99\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 99);
}

#[test]
fn match_enum_zero_payload_returns_arm_value() {
    let src = "enum Tile { Empty, Wall }\nfn main() -> Int {\n  var t = Empty\n  match t {\n    Empty => 1\n    Wall => 2\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn match_enum_payload_binding() {
    let src = "enum Tile { Empty, Number(Int) }\nfn main() -> Int {\n  var t = Number(42)\n  match t {\n    Empty => 0\n    Number(n) => n\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

// --- pattern-matching completeness (nested + literal patterns) --------------

#[test]
fn match_literal_int() {
    // Literal Int patterns test each value:
    // `match n { 1 => 10, 2 => 20, _ => 0 }`.
    let src = "fn main() -> Int {\n  var n = 2\n  match n {\n    1 => 10\n    2 => 20\n    _ => 0\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 20);
}

#[test]
fn match_literal_int_default() {
    // The wildcard arm must catch unmatched literals.
    let src = "fn main() -> Int {\n  var n = 99\n  match n {\n    1 => 10\n    2 => 20\n    _ => 0\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

#[test]
fn match_literal_int_first_arm() {
    // Matching the first literal arm.
    let src = "fn main() -> Int {\n  var n = 1\n  match n {\n    1 => 10\n    2 => 20\n    _ => 0\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 10);
}

#[test]
fn match_bool() {
    // Bool patterns: `match b { true => 1, false => 0 }`.
    let src =
        "fn main() -> Int {\n  var b = true\n  match b {\n    true => 1\n    false => 0\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn match_nested_variant_pattern() {
    // Nested pattern: `Wrapped(Some(n))` extracts through two layers of variant,
    // so lowering must recurse into sub-patterns.
    let src = "enum Inner { None, Some(Int) }\nenum Outer { Wrapped(Inner), Bare }\nfn main() -> Int {\n  var v = Wrapped(Some(7))\n  match v {\n    Wrapped(Some(n)) => n\n    Wrapped(None) => 0\n    Bare => 1\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 7);
}

#[test]
fn match_nested_variant_none_branch() {
    // The same nested match but matching the inner None.
    let src = "enum Inner { None, Some(Int) }\nenum Outer { Wrapped(Inner), Bare }\nfn main() -> Int {\n  var v = Wrapped(None)\n  match v {\n    Wrapped(Some(n)) => n\n    Wrapped(None) => 0\n    Bare => 1\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

/// The usefulness matrix pairs each pattern column with a type, so lowering pads
/// a variant pattern to its payload arity — which puts a
/// `TypedPattern::Wildcard` in a *payload slot* of MIR's decision tree.
///
/// Each spelling below selects the same arm and must read the same value: a
/// padded slot is extracted and discarded, not skipped and not read off the end.
#[test]
fn a_padded_payload_wildcard_selects_its_arm_at_runtime() {
    let enums = "enum Inner { Nil, Val(Int) }\nenum Outer { Wrapped(Inner), Bare }\n";
    for arm in ["Wrapped(_)", "Wrapped(i)"] {
        let src = format!(
            "{enums}fn main() -> Int {{\n  var v = Wrapped(Val(7))\n  \
             match v {{\n    {arm} => 5\n    Bare => 1\n  }}\n}}\n"
        );
        let (rt, result) = run_main(&src);
        assert!(!rt.has_pending_fault(), "`{arm}` faulted: {:?}", rt.fault());
        assert_eq!(result.as_int(), 5, "`{arm}` must take the first arm");
    }

    // A wildcard in a *nested* payload slot emits a payload read the arm never
    // uses. The value it selects must be unchanged, and the discarded slot must
    // not fault. (ADR-134 makes the bare spelling `Wrapped(Val)` a `Y124`;
    // `Val(_)` is the same pattern and the same padding, said out loud.)
    let src = format!(
        "{enums}fn main() -> Int {{\n  var v = Wrapped(Val(7))\n  \
         match v {{\n    Wrapped(Nil) => 1\n    Wrapped(Val(_)) => 2\n    Bare => 3\n  }}\n}}\n"
    );
    let (rt, result) = run_main(&src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(
        result.as_int(),
        2,
        "`Val(_)` reads and discards the payload"
    );

    // …and the padded arm is still a *test*: the other payload constructor
    // takes its own arm rather than falling into the padded one.
    let src = format!(
        "{enums}fn main() -> Int {{\n  var v = Wrapped(Nil)\n  \
         match v {{\n    Wrapped(Val(_)) => 2\n    Wrapped(Nil) => 1\n    Bare => 3\n  }}\n}}\n"
    );
    let (rt, result) = run_main(&src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1, "`Nil` is not caught by `Val(_)`");
}

#[test]
fn match_multi_payload_binding() {
    // A variant with multiple payload fields, all bound.
    let src = "enum Shape { Point(Int, Int), Empty }\nfn main() -> Int {\n  var s = Point(3, 4)\n  match s {\n    Point(x, y) => x + y\n    Empty => 0\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 7);
}

#[test]
fn match_variable_bind_whole_scrutinee() {
    // A bare variable bind `x` matches anything and binds the whole value.
    let src = "enum Tile { Empty, Number(Int) }\nfn main() -> Int {\n  var t = Number(5)\n  match t {\n    x => 99\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 99);
}

// --- closures (§4.10) -------------------------------------------------------
//
// Closures capture outer `var`/`param` values by copying them into the closure's
// runtime environment; the synthetic function loads them at entry. Calling a
// closure value is an indirect call through its `fn_ptr`.

#[test]
fn closure_no_captures() {
    // A closure that references only its own param.
    let (rt, result) = run_main("fn main() -> Int { var f = |x| x * 2; f(21) }");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

#[test]
fn closure_captures_outer_let() {
    // The headline demo: `var o = 10; var f = |x| x + o; f(5)` → 15.
    let (rt, result) = run_main("fn main() -> Int { var o = 10; var f = |x| x + o; f(5) }");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 15);
}

#[test]
fn closure_captures_multiple() {
    // Two captures, used together.
    let (rt, result) =
        run_main("fn main() -> Int { var a = 3; var b = 4; var f = |x| x + a * b; f(5) }");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 17);
}

#[test]
fn closure_captures_param_of_enclosing_fn() {
    // The captured value is the enclosing fn's param.
    let (rt, result) = run_main(
        "fn make(o: Int) -> Int { var f = |x| x + o; f(5) }\nfn main() -> Int { make(10) }",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 15);
}

#[test]
fn closure_returned_and_called() {
    // A fn returns a closure; main calls it. Exercises capture across a fn
    // boundary (the closure outlives `make`'s frame — the env is GC'd).
    let (rt, result) = run_main(
        "fn make(o: Int) -> Int { |x| x + o }\nfn main() -> Int { var f = make(10); f(5) }",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 15);
}

#[test]
fn closure_curried() {
    // A closure returning a closure: |x| |y| x + y. The inner closure captures
    // the outer's param `x`.
    //
    // `x` is the *outer's param*, so the outer closure captures nothing and
    // there is no environment to hand down. The transitive case below is the one
    // that needs a name declared outside **both**.
    let (rt, result) =
        run_main("fn main() -> Int { var add = |x| |y| x + y; var inc = add(1); inc(41) }");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

// --- Transitive captures ----------------------------------------------------
//
// A closure whose body *is* a closure must capture whatever the returned one
// names from outside them both: the returned closure's environment is filled
// from the returning closure's frame, so the returning closure has to be
// holding the value. `capture.rs`'s walker therefore descends into a body that
// is itself a closure; recording nothing there fills the inner env slot from an
// empty environment with `Unit`. That one slot fails three ways — a panic, a
// SIGSEGV, and a silently wrong answer — so each gets its own test.

#[test]
fn closure_curried_captures_transitively() {
    // `base` is declared outside both closures. The panic mode: an empty
    // environment makes `praxis_int_load` read a `Unit` as an `Int`.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var base = 10\n  var mk = |a| |b| b + base\n  mk(5)(1)\n}\n",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 11);
}

#[test]
fn closure_curried_three_levels_captures_transitively() {
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var base = 10\n  var mk = |a| |b| |c| c + base\n  mk(1)(2)(3)\n}\n",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 13);
}

#[test]
fn a_transitively_captured_text_is_the_text_and_not_unit() {
    // The silent mode, and the reason this needs a value assertion rather than a
    // `has_pending_fault` check: a captured `Text` read back as `Unit` neither
    // panics nor faults — `.len()` simply answers 0, with nothing to say so.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var base = \"hello\"\n  var mk = |a| |b| base\n  mk(0)(0).len()\n}\n",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 5);
}

#[test]
fn a_bare_and_a_braced_curried_body_agree() {
    // The two spellings differ by one pair of braces, and must agree.
    let (rt_bare, bare) = run_main(
        "fn main() -> Int {\n  var base = 10\n  var mk = |a| |b| b + base\n  mk(5)(1)\n}\n",
    );
    assert!(!rt_bare.has_pending_fault(), "fault: {:?}", rt_bare.fault());
    let (rt_braced, braced) = run_main(
        "fn main() -> Int {\n  var base = 10\n  var mk = |a| { |b| b + base }\n  mk(5)(1)\n}\n",
    );
    assert!(
        !rt_braced.has_pending_fault(),
        "fault: {:?}",
        rt_braced.fault()
    );
    assert_eq!(bare.as_int(), braced.as_int());
    assert_eq!(bare.as_int(), 11);
}

// --- mutable captures via VarCell (§4.10) -----------------------------------
//
// A `var` captured by a closure is boxed into a GC'd VarCell at its binding
// site. The binding function and every capturing closure share the cell, so a
// mutation in one frame is visible to the other.

#[test]
fn closure_reads_mutable_capture() {
    // The closure reads (but does not write) a captured `var` — the cell holds
    // the initial value.
    let (rt, result) = run_main("fn main() -> Int {\n  var c = 10\n  var f = |_| c\n  f(0)\n}\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 10);
}

#[test]
fn closure_mutates_mutable_capture_visible_outside() {
    // The headline mutable-capture scenario: a closure mutates the captured
    // `var`, and the outer scope reads the updated value after the closure runs.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var counter = 0\n  var inc = |_| { counter = counter + 1 }\n  inc(0)\n  inc(0)\n  counter\n}\n",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn closure_compound_assign_on_mutable_capture() {
    // A compound assignment (`+=`) on a captured `var` inside a closure: read
    // the cell, add, write back.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var total = 100\n  var add = |n| { total += n }\n  add(5)\n  add(10)\n  total\n}\n",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 115);
}

#[test]
fn mutable_capture_survives_returned_closure() {
    // A closure capturing a `var` is returned from a fn; calling it mutates the
    // cell (which outlives the fn's frame — it's GC'd).
    let (rt, result) = run_main(
        "fn make() {\n  var n = 0\n  |x| { n = n + x; n }\n}\nfn main() -> Int {\n  var bump = make()\n  bump(3)\n  bump(4)\n}\n",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 7);
}

#[test]
fn transitive_mutable_capture_shares_one_cell_across_frames() {
    // The SIGSEGV mode: a reassigned `var` is a `ByCell` capture, and an empty
    // environment hands the inner closure a `Unit` where a `VarCell` pointer is
    // expected, which the write dereferences. What must hold is stronger than
    // "no crash": the *same* cell is threaded through every environment, so a
    // write two levels down is visible at the top.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var n = 0\n  var mk = |a| |b| { n = n + a + b; n }\n  mk(1)(2)\n  n = n + 50\n  mk(3)(4)\n  n\n}\n",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // 0 + 1 + 2 = 3, then +50 = 53, then +3+4 = 60.
    assert_eq!(result.as_int(), 60);
}

#[test]
fn transitive_mutable_capture_shares_one_cell_across_three_frames() {
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var n = 0\n  var mk = |a| |b| |c| { n = n + a + b + c; n }\n  mk(1)(2)(3)\n  n = n + 50\n  mk(1)(2)(3)\n  n\n}\n",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 62);
}

// --- monomorphization (§13.6) -----------------------------------------------
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

/// One generic function applied to `Option[Int]` and to `Option[Text]` needs
/// **two** specializations.
///
/// The mono cache key has to carry the element type: if both call sites hash to
/// `id__Option`, the second runs the first's `Int` clone over a `Text` payload.
#[test]
fn monomorphization_distinguishes_option_element_types() {
    let src = "fn id(x) { x }\n\
               fn main() -> Int {\n  \
                 var a = id(Some(7))\n  \
                 var b = id(Some(\"hi\"))\n  \
                 var n = match a {\n    Some(v) => v\n    None => 0\n  }\n  \
                 var s = match b {\n    Some(v) => v\n    None => \"\"\n  }\n  \
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
    let (rt, result) = run_main("fn id(x) { x }\nfn main() -> Int { var f = |n| id(n); f(42) }");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

// ===========================================================================
// Adversarial edge-case tests — pipeline fusion, closures, GC interactions.
//
// These tests probe combinations: mutable captures mutated inside fused loops,
// GC pressure mid-pipeline, nested closure allocation during fusion,
// fold/reduce over GC-object accumulators, take(0)/negative-literal edges, and
// object-valued (non-Int) pipeline elements. Written from an adversary's
// perspective: try to break it even when it "should" work.
// ===========================================================================

/// Helper: build Praxis source that constructs a Vec of `n` sequential ints
/// starting at `start`, as a sequence of `v.push(...)` statements bound to `v`.
/// (`.push()` returns Unit, so it cannot be chained.)
fn vec_of(start: i64, n: i64) -> String {
    let mut s = String::from("var v = Vec()");
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
        "fn main() -> Int {{\n  var counter = 0\n  {vec}\n  var out = v.map(|x| {{ counter += x; x }})\n  counter\n}}\n",
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
    let src = "fn main() -> Int {\n  var counter = 0\n  var v = Vec()\n  var i = 0\n  while i < 300 { v.push(i); i = i + 1 }\n  var out = v.map(|x| { counter += x; x })\n  counter\n}\n";
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
    let src = "fn main() -> Int {\n  var v = Vec()\n  var i = 0\n  while i < 300 { v.push(i); i = i + 1 }\n  var out = v.map(|x| x * 2)\n  out.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 300);
}

#[test]
fn adv_pipeline_collect_vec_elements_survive_gc_stress() {
    // Collect into a Vec under heavy GC pressure, then sum the collected Vec
    // in a *separate* step. If the collect_vec's freshly-pushed elements are
    // not properly rooted, the second sum reads garbage / freed objects.
    let src = "fn main() -> Int {\n  var v = Vec()\n  var i = 0\n  while i < 300 { v.push(i); i = i + 1 }\n  var out = v.map(|x| x * 3)\n  var sum = 0\n  var j = 0\n  while j < out.len() { sum += out.get(j); j = j + 1 }\n  sum\n}\n";
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
    // across every iteration's GC.
    let src = "fn main() -> Int {\n  var v = Vec()\n  var i = 0\n  while i < 500 { v.push(i); i = i + 1 }\n  v.fold(0, |a, x| a + x)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // sum(0..=499) = 499*500/2 = 124750
    assert_eq!(result.as_int(), 124750);
}

#[test]
fn adv_pipeline_fold_into_vec_now_supported() {
    // Fold into a Vec accumulator. The closure param `a` is inferred as Vec[Int]
    // from the init `Vec()`: bidirectional inference threads the combinator's
    // accumulator type (the name-shared `Acc` in fold's signature) into the
    // closure's params before the body is inferred, so `a.push(x)` resolves.
    // Collects [1,2] → len 2.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  var acc = v.fold(Vec(), |a, x| { a.push(x); a })\n  acc.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn adv_pipeline_reduce_into_int_accumulator() {
    // reduce over Ints under GC pressure. The Reduce sink seeds from the first
    // element then folds. Verifies the seen-flag + Gc acc survive the loop.
    let src = "fn main() -> Int {\n  var v = Vec()\n  var i = 0\n  while i < 200 { v.push(i); i = i + 1 }\n  v.reduce(|a, x| a + x)\n}\n";
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
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  var fs = v.map(|x| |y| x + y)\n  fs.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

#[test]
fn adv_pipeline_curried_closure_captures_transitively() {
    // The shape above allocates a nested closure per iteration but captures
    // nothing from outside the pipeline, so it does not exercise the transitive
    // path. Here `base` is declared outside both closures *and* outside the
    // fused loop: `[1,2,3].map(|x| |y| x + y + base)`.
    let src = "fn main() -> Int {\n  var base = 100\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  var fs = v.map(|x| |y| x + y + base)\n  fs[0](1) + fs[2](1)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // (1 + 1 + 100) + (3 + 1 + 100) = 102 + 104 = 206.
    assert_eq!(result.as_int(), 206);
}

#[test]
fn adv_pipeline_nested_closure_allocation_gc_stress() {
    // Same as above but 200 elements: each map call allocates a closure with
    // a captured Int env. The captured env objects must survive GC across the
    // rest of the loop while the collect_vec accumulates them.
    let src = "fn main() -> Int {\n  var v = Vec()\n  var i = 0\n  while i < 200 { v.push(i); i = i + 1 }\n  var fs = v.map(|x| |y| x + y)\n  fs.len()\n}\n";
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
    // runs.
    let src = "fn main() -> Int {\n  var v = Vec()\n  var i = 0\n  while i < 200 {\n    var inner = Vec()\n    inner.push(i)\n    inner.push(i)\n    v.push(inner)\n    i = i + 1\n  }\n  v.map(|inner| inner).count()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // 200 inner Vecs collected
    assert_eq!(result.as_int(), 200);
}

#[test]
fn adv_pipeline_method_on_closure_param_from_collection_now_supported() {
    // A method call on a closure parameter whose type is the element type of a
    // collection — `v.map(|i| i.len())` over a Vec[Vec[Int]] — resolves through
    // two mechanisms:
    //
    // 1. Bidirectional inference threads the receiver's element type into the
    //    closure param before the body is inferred, so `.len()` resolves once
    //    the element type is known.
    // 2. The HM value restriction: `var v = Vec()` does not generalize to
    //    `forall T. Vec[T]` (an expansive RHS is left monomorphic), so the
    //    element-type pinning from `v.push(inner)` propagates to the later
    //    `v.map(...)` instead of each call instantiating a fresh element type.
    //
    // inner = [1] → len 1 → sum = 1.
    let src = "fn main() -> Int {\n  var v = Vec()\n  var inner = Vec()\n  inner.push(1)\n  v.push(inner)\n  v.map(|i| i.len()).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_pipeline_min_by_on_nested_collection_lengths() {
    // `.len()`-based min_by/max_by comparators over nested collections: find the
    // inner Vec with the fewest elements. The comparator closure's params `a`/`b`
    // are the element type Vec[Int], pinned by bidirectional inference + the
    // value restriction so `a.len()`/`b.len()` resolve. min_by takes a
    // (T,T)->Bool comparator; "shorter" is
    // `a.len() < b.len()`. inner lengths [3,1,2] → shortest = b → len 1.
    let src = String::from("fn main() -> Int {\n")
        + "  var v = Vec()\n"
        + "  var a = Vec()\n  a.push(1)\n  a.push(2)\n  a.push(3)\n  v.push(a)\n"
        + "  var b = Vec()\n  b.push(9)\n  v.push(b)\n"
        + "  var c = Vec()\n  c.push(4)\n  c.push(5)\n  v.push(c)\n"
        + "  var shortest = v.min_by(|a, b| a.len() < b.len())\n"
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
    let src = "fn main() -> Int {\n  var v = Vec()\n  var a = Vec()\n  a.push(10)\n  a.push(20)\n  v.push(a)\n  var b = Vec()\n  b.push(30)\n  v.push(b)\n  v.map(|inner| inner).len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // 2 inner Vecs collected
    assert_eq!(result.as_int(), 2);
}

#[test]
fn adv_pipeline_find_with_allocating_predicate() {
    // find's predicate allocates (creates an Int) before returning its bool.
    // If the fused loop doesn't root the current element across the predicate's
    // allocation, find matches the wrong element or faults. The answer is the
    // *element*, which the loop has to keep alive across that allocation to
    // hand back at all.
    let src = "fn main() -> Int {\n  var v = Vec()\n  var i = 0\n  while i < 100 { v.push(i); i = i + 1 }\n  match v.find(|x| x + 0 == 50) { Some(n) => n, None => -1 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 50);
}

#[test]
fn adv_pipeline_any_short_circuits_keeps_loop_invariant() {
    // any short-circuits; verify the break leaves the source Vec intact (no
    // corruption from the fused loop's bookkeeping) by summing it afterwards.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  var b = v.any(|x| x == 3)\n  var after = 0\n  var i = 0\n  while i < v.len() { after += v.get(i); i = i + 1 }\n  if b { after } else { 0 }\n}\n";
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
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  var a = v.map(|x| x * 10).sum()\n  var b = v.map(|x| x * 100).sum()\n  a + b\n}\n";
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
    let src = "fn main() -> Int {\n  var v = Vec()\n  var i = 0\n  while i < 500 { v.push(i); i = i + 1 }\n  v.min_by(|a, b| a < b)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // min is 0
    assert_eq!(result.as_int(), 0);
}

#[test]
fn adv_pipeline_max_by_under_gc_pressure() {
    // max_by over 500 Ints under GC pressure.
    let src = "fn main() -> Int {\n  var v = Vec()\n  var i = 0\n  while i < 500 { v.push(i); i = i + 1 }\n  v.max_by(|a, b| a < b)\n}\n";
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
    let src = "fn main() -> Int {\n  var v = Vec()\n  var i = 1\n  while i <= 500 { v.push(i); i = i + 1 }\n  v.min()\n}\n";
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
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.push(4)\n  v.push(5)\n  v.push(6)\n  v.push(7)\n  v.map(|x| x + 1).filter(|x| x > 3).map(|x| x * 2).filter(|x| x < 15).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 44);
}

#[test]
fn adv_pipeline_flat_map_gc_stress_preserves_inner_vecs() {
    // flat_map under GC stress: each closure call allocates a fresh Vec, the
    // inner loop reads it. If the inner Vec isn't rooted, the inner loop
    // faults or reads freed memory. 100 outer × 3 inner = 300 sum if i.
    let src = "fn main() -> Int {\n  var v = Vec()\n  var i = 0\n  while i < 100 { v.push(i); i = i + 1 }\n  v.flat_map(|x| {\n    var r = Vec()\n    r.push(x)\n    r.push(x)\n    r.push(x)\n    r\n  }).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // sum(0..=99) * 3 = 4950 * 3 = 14850
    assert_eq!(result.as_int(), 14850);
}

#[test]
fn adv_pipeline_empty_source_collect_is_empty_vec() {
    // Empty source → collect → empty Vec → len 0. Verifies the Collect sink's
    // collect_vec is allocated and returned even when the loop body never runs.
    let src = "fn main() -> Int {\n  var v = Vec()\n  var out = v.map(|x| x * 2)\n  out.len()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

/// An empty `min`/`max` faults; it does not answer `0`.
///
/// `0` is not a *missing* answer, it is a **wrong** one: it is below every
/// element of `[3, 4]` and above every element of `[-3, -4]`, so nothing at the
/// call site can tell a seeded accumulator from a real minimum.
///
/// `reduce`, `min_by` and `max_by` fault here too, rather than answering
/// `Option`: an empty `min` is a mistake in the program and not the
/// domain-level absence §4.7 reserves `Option` for.
#[test]
fn an_empty_min_or_max_faults_rather_than_answering_zero() {
    for sink in ["min", "max"] {
        let src = format!("fn main() -> Int {{\n  var v = Vec()\n  v.{sink}()\n}}\n");
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
        "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  \
         v.filter(|x| x > 100).min()\n}\n",
    );
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::EmptyCollection);

    // …and a non-empty one still answers, so the guard is the empty case and
    // not a new refusal.
    let (rt, result) =
        run_main("fn main() -> Int {\n  var v = Vec()\n  v.push(3)\n  v.push(4)\n  v.min()\n}\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var v = Vec()\n  v.push(0 - 3)\n  v.push(0 - 4)\n  v.max()\n}\n",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), -3, "the seeded 0 was above every element");
}

#[test]
fn adv_pipeline_empty_source_any_is_false() {
    // Empty source → any → false (vacuously). Packed as 0.
    let src = "fn main() -> Int {\n  var v = Vec()\n  var b = v.any(|x| x == 0)\n  if b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

#[test]
fn adv_pipeline_empty_source_all_is_true() {
    // Empty source → all → true (vacuously). Packed as 1.
    let src = "fn main() -> Int {\n  var v = Vec()\n  var b = v.all(|x| x > 0)\n  if b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn adv_pipeline_empty_source_reduce() {
    // Empty source → reduce. `reduce` seeds from the first element, and there
    // is none — so the answer is a fault, not whatever the unseeded Gc slot
    // happened to hold.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.reduce(|a, x| a + x)\n}\n";
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
    let src = "fn mk(off: Int) { |x| x + off }\nfn main() -> Int {\n  var f = mk(100)\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.map(f).sum()\n}\n";
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
        "fn main() -> Int {{\n  {vec}\n  var a = v.sum()\n  var b = v.sum()\n  a + b\n}}\n",
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
    let src = "fn main() -> Int {\n  var v = Vec()\n  var i = 0\n  while i < 200 { v.push(i); i = i + 1 }\n  v.take(50).count()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 50);
}

#[test]
fn adv_pipeline_zip_under_gc_pressure() {
    // zip of two 300-element Vecs, count the pairs. Both source Vecs and the
    // index must survive GC.
    let src = "fn main() -> Int {\n  var a = Vec()\n  var b = Vec()\n  var i = 0\n  while i < 300 { a.push(i); b.push(i); i = i + 1 }\n  a.zip(b).count()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 300);
}

#[test]
fn adv_pipeline_take_while_then_collect_under_gc_pressure() {
    // take_while under GC pressure: stops at the first element >= 50, collects.
    let src = "fn main() -> Int {\n  var v = Vec()\n  var i = 0\n  while i < 200 { v.push(i); i = i + 1 }\n  v.take_while(|x| x < 50).count()\n}\n";
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
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(9223372036854775807)\n  v.push(0)\n  v.push(1)\n  v.sum()\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "sum overflow should fault");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::IntOverflow);
}

#[test]
fn adv_fused_map_closure_fault_propagates() {
    // A map closure faults (div-by-zero on element 2). The fault must propagate
    // through the fused loop's CallIndirect + check_fault without the loop
    // continuing or the host crashing.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(10)\n  v.push(0)\n  v.push(30)\n  v.map(|x| 100 / x).sum()\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "div-by-zero in map should fault");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::DivByZero);
}

#[test]
fn adv_fused_filter_predicate_fault_propagates() {
    // A filter predicate faults. Verifies fault propagation through the
    // predicate's CallIndirect + the filter stage's branch structure.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(5)\n  v.push(0)\n  v.filter(|x| 100 / x > 1).count()\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "div-by-zero in filter should fault");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::DivByZero);
}

#[test]
fn adv_fused_fold_closure_fault_propagates() {
    // A fold closure faults mid-fold. Verifies the Fold sink's CallIndirect
    // fault check works and the accumulator isn't left corrupted.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(0)\n  v.fold(0, |a, x| a + 100 / x)\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "div-by-zero in fold should fault");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::DivByZero);
}

#[test]
fn adv_fused_find_predicate_fault_propagates() {
    // A find predicate faults. Verifies short-circuit sink fault propagation.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(0)\n  v.find(|x| 10 / x == 1)\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "div-by-zero in find should fault");
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::DivByZero);
}

#[test]
fn adv_nested_closures_share_var_cell() {
    // Two closures capture the same `var`; calling one then the other observes
    // the shared cell. inc mutates, getn returns it.
    let src = "fn main() -> Int {\n  var n = 0\n  var inc = |x| { n = n + x }\n  var getn = |_| n\n  inc(10)\n  inc(5)\n  getn(0)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 15);
}

#[test]
fn adv_nested_closures_share_var_cell_under_gc_pressure() {
    // Same as above but allocate heavily between calls so GC runs while both
    // closures' envs (pointing at the same VarCell) must survive.
    let src = "fn main() -> Int {\n  var n = 0\n  var inc = |x| { n = n + x }\n  var getn = |_| n\n  inc(10)\n  var i = 0\n  var garbage = Vec()\n  while i < 500 { garbage.push(i); i = i + 1 }\n  inc(5)\n  getn(0)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 15);
}

#[test]
fn adv_closure_mutating_capture_then_returned_and_called_repeatedly() {
    // A returned closure mutates its captured `var` each call; called 100× under
    // GC pressure. The VarCell must survive across every call's potential GC.
    let src = "fn make() {\n  var n = 0\n  |x| { n = n + x; n }\n}\nfn main() -> Int {\n  var bump = make()\n  var i = 0\n  while i < 100 { bump(1); i = i + 1 }\n  bump(0)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 100);
}

#[test]
fn adv_recursive_function_with_captured_var() {
    // A closure that captures a `var` and recurses (via a named fn, since
    // recursive closures aren't specially handled). The VarCell must survive
    // the recursion's GC pressure.
    let src = "fn count(n: Int, dec) -> Int {\n  if n == 0 { dec(0) } else { dec(1); count(n - 1, dec) }\n}\nfn main() -> Int {\n  var total = 0\n  var add = |x| { total += x }\n  count(100, add)\n  total\n}\n";
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
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  var sum = 0\n  for x in v {\n    sum = sum + x\n  }\n  sum\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 6);
}

#[test]
fn adv_for_loop_over_vec_under_gc_pressure() {
    // for-loop iterating a 500-element Vec, summing. The loop counter and the
    // source Vec must survive GC during iteration.
    let src = "fn main() -> Int {\n  var v = Vec()\n  var i = 0\n  while i < 500 { v.push(i); i = i + 1 }\n  var sum = 0\n  for x in v { sum += x }\n  sum\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // sum(0..=499) = 124750
    assert_eq!(result.as_int(), 124750);
}

#[test]
fn adv_pipeline_chain_after_pipeline_chain_nested() {
    // A pipeline whose source is itself a pipeline result that was collected:
    // `(v.map(f)).filter(p).sum()`. This variant uses a capturing closure in the
    // inner map AND a predicate in the outer filter, both reading the same
    // captured `var`. Verifies two closures + a shared cell all root correctly
    // in one fused loop.
    let src = "fn main() -> Int {\n  var threshold = 5\n  var v = Vec()\n  var i = 0\n  while i < 20 { v.push(i); i = i + 1 }\n  v.map(|x| x + threshold).filter(|x| x > threshold).sum()\n}\n";
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
    let src = "fn main() -> Int {\n  var adder = |off| |x| x + off\n  var add10 = adder(10)\n  var v = Vec()\n  var i = 0\n  while i < 200 { v.push(i); i = i + 1 }\n  v.map(add10).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // sum(0..=199) + 10*200 = 19900 + 2000 = 21900
    assert_eq!(result.as_int(), 21900);
}

#[test]
fn adv_mutable_capture_read_in_predicate_of_fused_filter() {
    // A filter predicate reads a captured `var` (not mutating). The VarCell
    // read must work inside the fused filter stage.
    let src = "fn main() -> Int {\n  var limit = 10\n  var v = Vec()\n  var i = 0\n  while i < 30 { v.push(i); i = i + 1 }\n  v.filter(|x| x > limit).count()\n}\n";
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
    let src = "fn main() -> Int {\n  var limit = 5\n  var setlimit = |n| { limit = n }\n  setlimit(15)\n  var v = Vec()\n  var i = 0\n  while i < 30 { v.push(i); i = i + 1 }\n  v.filter(|x| x > limit).count()\n}\n";
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
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.flat_map(|x| Vec()).count()\n}\n";
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
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.flat_map(|x| {\n    var r = Vec()\n    r.push(x)\n    r.push(x * 10)\n    r\n  }).filter(|x| x > 5).sum()\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 30);
}

#[test]
fn adv_indirect_call_on_local_closure_works() {
    // A closure bound to a local, then called: the callee resolves to a local.
    // The closure-from-a-collection case is
    // `adv_call_closure_retrieved_from_collection` below.
    let src = "fn main() -> Int {\n  var f = |x| x + 7\n  f(100)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 107);
}

#[test]
fn adv_call_closure_retrieved_from_collection() {
    // Invoking a closure retrieved from a collection — `fs.get(0)(100)`. A
    // postfix `expr(args)` parse production wraps the preceding expression as
    // the call's callee; the HIR lowerer stores it as `callee_expr` and
    // inference unifies it against a Func; the MIR builder lowers it to
    // Inst::CallIndirect, which reads the closure's fn_ptr and calls through it.
    let src = String::from("fn main() -> Int {\n")
        + "  var fs = Vec()\n"
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
        + "  var mk = |a| |b| a + b\n"
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
        + "  var fs = Vec()\n"
        + "  fs.push(|x| x + 1)\n"
        + "  fs.push(|x| x * 100)\n"
        + "  var garbage = Vec()\n"
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
        "fn main() -> Int {\n  var a = 4\n  var show_old = |_| a\n  var a = 99\n  show_old(0)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // show_old captured the first a (4), not the shadowed a (99)
    assert_eq!(result.as_int(), 4);
}

#[test]
fn adv_shadowing_then_closure_captures_correct_binding_type_change() {
    // §4.2: the headline example — a closure created before a shadowing `var`
    // with a different type retains the original Int binding.
    let src = "fn main() -> Int {\n  var a = 4\n  var show_old = |_| a\n  var a = \"Foo\"\n  show_old(0)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // show_old captured the first a (Int 4), not the Text "Foo"
    assert_eq!(result.as_int(), 4);
}

#[test]
fn adv_shadowing_initializer_resolves_previous_binding() {
    // §5.3: a shadowing initializer resolves names in the preceding environment.
    // `var a = a + 1` — the RHS `a` is the previous binding.
    let src = "fn main() -> Int {\n  var a = 4\n  var a = a + 1\n  a\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 5);
}

#[test]
fn adv_let_shadowing_changes_type() {
    // §4.2: shadowing may change type. `var a = 4; var a = "x"` — both valid.
    let src = "fn main() -> Int {\n  var a = 4\n  var a = a + 1\n  a\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 5);
}

// --- Option[T] prelude enum -------------------------------------------------
// Option is a polymorphic prelude enum (forall T. Option[T]) with variants
// Some(T) and None. These tests exercise construction, matching, equality, and
// cross-site unification (structural same-named-enum unification).

#[test]
fn m9_option_some_construction_and_match() {
    // `Some(42)` constructs; matching `.Some(n)` extracts the payload.
    let src = "fn main() -> Int {\n  var v = Some(42)\n  match v {\n    Some(n) => n\n    None => 0\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

#[test]
fn m9_option_none_construction_and_match() {
    // `None` constructs a payload-less variant.
    let src = "fn main() -> Int {\n  var v = None\n  match v {\n    Some(n) => n\n    None => 7\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 7);
}

#[test]
fn m9_option_some_none_equality() {
    // `Some(1) == Some(1)` is true; `Some(1) == None` is false; `None == None` is true.
    let src = "fn main() -> Int {\n  var a = Some(1) == Some(1)\n  var b = Some(1) == None\n  var c = None == None\n  if a { if b { 3 } else { if c { 2 } else { 4 } } } else { 1 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

#[test]
fn m9_option_unifies_across_construction_sites() {
    // Two independently-constructed Some values unify through match + equality,
    // exercising the same-named-enum structural unification.
    let src = "fn main() -> Int {\n  var a = Some(10)\n  var b = Some(20)\n  if a == Some(10) { if b == Some(20) { 1 } else { 2 } } else { 3 }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn m9_option_text_payload() {
    // Option is polymorphic: Some(Text) works, not just Some(Int).
    let src = "fn main() -> Int {\n  var v = Some(\"hi\")\n  match v {\n    Some(s) => if s == \"hi\" { 1 } else { 0 }\n    None => 9\n  }\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn m9_option_type_annotation() {
    // An explicit `Option[Int]` annotation unifies with Some(5).
    let src = "fn main() -> Int {\n  var v: Option[Int] = Some(5)\n  match v {\n    Some(n) => n\n    None => 0\n  }\n}\n";
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

// --- named heterogeneous sections + repeated tail (§7.5) --------------------

#[test]
fn m9_named_sections_two_fields() {
    // sections(rules: ..., updates: ...) → record { rules, updates }.
    // rules = Vec[Int] of 2 values; updates = Vec[Int] of 3 values.
    // Access `.a` and `.b` field on the record.
    let src = "fn main() -> Int {\n  var data = read sections(a: lines(int), b: lines(int))\n  data.a.len() + data.b.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "1\n2\n\n3\n4\n5");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 5); // 2 + 3
}

#[test]
fn m9_named_sections_field_values() {
    // The first section's first value is 7.
    let src = "fn main() -> Int {\n  var data = read sections(a: lines(int), b: lines(int))\n  data.a.get(0)\n}\n";
    let (rt, result) = run_main_with_input(src, "7\n8\n\n9\n10");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 7);
}

#[test]
fn m9_named_sections_with_repeated_tail() {
    // sections(single: lines(int), rest: repeated(lines(int))) — one fixed
    // section then all remaining sections folded into a Vec[Vec[Int]].
    let src = "fn main() -> Int {\n  var data = read sections(single: lines(int), rest: repeated(lines(int)))\n  data.single.len() + data.rest.len()\n}\n";
    // 1 section of 1 line (single), then 3 sections (rest has 3 elements).
    let (rt, result) = run_main_with_input(src, "100\n\n1\n\n2\n\n3");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 4); // 1 + 3
}

#[test]
fn m9_named_sections_template_fields() {
    // Named sections with template parsers → record of records. Access the
    // inner record's fields directly through indexing.
    let src = "fn main() -> Int {\n  var data = read sections(p: lines(`{x:int},{y:int}`))\n  var first = data.p.get(0)\n  first.x + first.y\n}\n";
    let (rt, result) = run_main_with_input(src, "3,4");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 7);
}

#[test]
fn m9_named_sections_too_few_sections_faults() {
    // Fewer sections than named fields → ParseFailed fault.
    let src =
        "fn main() -> Int {\n  var data = read sections(a: lines(int), b: lines(int))\n  42\n}\n";
    let (rt, _result) = run_main_with_input(src, "1\n2\n3"); // one section, two fields wanted
    assert!(
        rt.has_pending_fault(),
        "expected ParseFailed on too-few sections, got: {:?}",
        rt.fault()
    );
}

// --- block (§7.5, §7.7) -----------------------------------------------------

#[test]
fn m9_block_template_plus_named_field() {
    // sections(block(`Monkey {id:int}:`, items: lines(int))) — each section is
    // a block: a positional header template (flattening `id`) + a named `items`
    // field consuming the remaining lines.
    let src = "fn main() -> Int {\n  var monkeys = read sections(block(`Monkey {id:int}:`, items: lines(int)))\n  var m0 = monkeys.get(0)\n  m0.id + m0.items.len()\n}\n";
    let input = "Monkey 1:\n10\n20\n\nMonkey 2:\n30\n40";
    let (rt, result) = run_main_with_input(src, input);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // m0.id = 1, m0.items has 2 entries (10, 20) → 1 + 2 = 3
    assert_eq!(result.as_int(), 3);
}

#[test]
fn m9_block_second_section() {
    // The second monkey's id and item count.
    let src = "fn main() -> Int {\n  var monkeys = read sections(block(`Monkey {id:int}:`, items: lines(int)))\n  var m1 = monkeys.get(1)\n  m1.id + m1.items.len()\n}\n";
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
    let src = "fn main() -> Int {\n  var b = read block(`{x:int},{y:int}\\n{z:int}`)\n  b.x + b.y + b.z\n}\n";
    let (rt, result) = run_main_with_input(src, "1,2\n3");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 6);
}

#[test]
fn m9_block_template_that_writes_a_newline_spans_the_lines_it_writes() {
    // ADR-090. Both halves of the window in one number: the `\n` the first
    // template writes buys it a second line, and the final `{w:rest}` is
    // bounded to its own line.
    //
    // Both failure modes are silent or misplaced. Without `block_item_window`
    // the answer is 11, because the unbounded `{w:rest}` takes `"abcd\n"` (5)
    // instead of `"abcd"` (4) — a wrong answer, not a fault, which is why this
    // test is worth having. With a window of exactly one line the template's own
    // `\n` part has no terminator left inside its window.
    let src =
        "fn main() -> Int {\n  var b = read block(`{x:int},{y:int}\\n{z:int}`, `{w:rest}`)\n  \
               b.x + b.y + b.z + b.w.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "1,2\n3\nabcd\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // 1 + 2 + 3 + len("abcd") = 10, not 11.
    assert_eq!(result.as_int(), 10);
}

// --- choice generated enums (§7.5) ------------------------------------------

#[test]
fn m9_choice_first_alternative_matches() {
    // choice(A: `{a:int}`, B: `{b:int}`) on "42" → first case wins (.A).
    // Scalar payload recovered directly.
    let src = "fn main() -> Int {\n  var v = read choice(A: int, B: int)\n  match v {\n    A(n) => n\n    B(n) => n\n  }\n}\n";
    let (rt, result) = run_main_with_input(src, "42");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

#[test]
fn m9_choice_second_alternative_via_backtrack() {
    // choice(A: int, B: word) on "hello" — A fails (not an int), B wins via
    // backtracking. Scalar payloads keep this test about *backtracking*;
    // record-payload field access is covered by
    // `a_choice_payload_records_fields_are_readable`.
    let src = "fn main() -> Int {\n  var v = read choice(A: int, B: word)\n  match v {\n    A(n) => n\n    B(w) => 99\n  }\n}\n";
    let (rt, result) = run_main_with_input(src, "hello");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 99);
}

#[test]
fn m9_choice_distinct_payloads() {
    // Two cases with different scalar payload types; the matched case's payload
    // is recovered.
    let src = "fn main() -> Int {\n  var v = read choice(Num: int, Txt: word)\n  match v {\n    Num(n) => n\n    Txt(w) => 7\n  }\n}\n";
    let (rt, result) = run_main_with_input(src, "123");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 123);
}

#[test]
fn m9_choice_equality() {
    // Two choice results of the same shape compare equal when same variant+payload.
    let src = "fn main() -> Int {\n  var a = read choice(N: int)\n  var b = read choice(N: int)\n  if a == b { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main_with_input(src, "5");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

/// A `choice(...)` payload record's fields are readable from a match-bound
/// variant payload.
///
/// The payload record type is real end to end (ADR-024/ADR-025), and
/// `infer_variant_pattern` must reach the enum without going through the
/// constructor *symbol*, which an anonymous enum does not have (ADR-091
/// Decision 1). Otherwise `p` keeps an unbound variable, `p.a` lowers to `Unit`,
/// and the multiply that follows aborts the process rather than failing an
/// assertion.
#[test]
fn a_choice_payload_records_fields_are_readable() {
    // Through a binding: the payload record reaches the field read.
    let src = "fn main() -> Int {\n  var v = read choice(A: `{a:int},{b:int}`)\n  \
               match v {\n    A(p) => p.a * p.b\n  }\n}\n";
    let (rt, result) = run_main_with_input(src, "6,7");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);

    // …and through the headless record pattern, which is the only spelling
    // available: the payload record is anonymous, so there is no name a head
    // could write.
    let src = "fn main() -> Int {\n  var v = read choice(A: `{a:int},{b:int}`)\n  \
               match v {\n    A({a, b}) => a * b\n  }\n}\n";
    let (rt, result) = run_main_with_input(src, "6,7");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

// --- optional + Option[T] integration (§7.5) --------------------------------

#[test]
fn m9_optional_present_returns_some() {
    // optional(int) on "42" → Some(42).
    let src = "fn main() -> Int {\n  var v = read optional(int)\n  match v {\n    Some(n) => n\n    None => 0\n  }\n}\n";
    let (rt, result) = run_main_with_input(src, "42");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

#[test]
fn m9_optional_absent_returns_none() {
    // optional(int) on "hello" → None (int parse fails). No fault raised.
    let src = "fn main() -> Int {\n  var v = read optional(int)\n  match v {\n    Some(n) => n\n    None => 7\n  }\n}\n";
    let (rt, result) = run_main_with_input(src, "hello");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 7);
}

#[test]
fn m9_optional_some_none_equality() {
    // Some(5) == Some(5) is true; Some(5) == None is false; None == None is true.
    let src = "fn main() -> Int {\n  var a = read optional(int)\n  var b = read optional(int)\n  var same = a == b\n  if same { 1 } else { 0 }\n}\n";
    let (rt, result) = run_main_with_input(src, "5");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn m9_optional_present_and_absent_differ() {
    // Each read re-parses the whole input (§7.10). a = optional(int) on "5" →
    // Some(5); b = optional(word) on "5" → Some("5"). Both Some; result is a's n.
    let src = "fn main() -> Int {\n  var a = read optional(int)\n  var b = read optional(word)\n  match a {\n    Some(n) => match b {\n      Some(w) => n\n      None => 99\n    }\n    None => 0\n  }\n}\n";
    let (rt, result) = run_main_with_input(src, "5");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 5);
}

// --- scan (§7.5, C.9) -------------------------------------------------------

#[test]
fn m9_scan_extracts_matches_in_order() {
    // scan(choice(Mul: `mul({a:int},{b:int})`)) over corrupted text — finds all
    // mul(a,b) in source order, ignoring other text. Counts the matches.
    let src = "fn main() -> Int {\n  var ms = read scan(choice(M: `mul({a:int},{b:int})`))\n  ms.len()\n}\n";
    let input = "xmul(2,3)ymul(4,5)don't()mul(6,7)";
    let (rt, result) = run_main_with_input(src, input);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

#[test]
fn m9_scan_extracts_payload_values() {
    // The match count, which is what this test is about. Reading the payload's
    // own fields is `a_choice_payload_records_fields_are_readable`.
    let src = "fn main() -> Int {\n  var ms = read scan(choice(M: `mul({a:int},{b:int})`))\n  ms.len()\n}\n";
    let input = "abc()mul(1,2)xyz";
    let (rt, result) = run_main_with_input(src, input);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

#[test]
fn m9_scan_no_matches_returns_empty_vec() {
    // scan on text with no matches → empty Vec, no fault.
    let src = "fn main() -> Int {\n  var ms = read scan(choice(M: `mul({a:int},{b:int})`))\n  ms.len()\n}\n";
    let input = "nothing here at all";
    let (rt, result) = run_main_with_input(src, input);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

// --- matrix, ragged grids, chars, one_of (§7.5) -----------------------------

#[test]
fn m9_one_of_matches_a_char() {
    // one_of("LR") on "L" → Char 'L'. Verify by counting via chars.
    let src =
        "fn main() -> Int {\n  var cs = read chars(one_of(\"LR\"), skip: none)\n  cs.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "LLRRL");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 5);
}

#[test]
fn m9_chars_skip_whitespace() {
    // chars(one_of("^v<>"), skip: whitespace) extracts moves ignoring spaces.
    let src = "fn main() -> Int {\n  var cs = read chars(one_of(\"^v<>\"), skip: whitespace)\n  cs.len()\n}\n";
    let (rt, result) = run_main_with_input(src, "^ v < > <");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 5);
}

#[test]
fn m9_matrix_rectangular_int() {
    // matrix(int) on whitespace-separated ints → Grid[Int]. Count cells = w*h.
    let src = "fn main() -> Int {\n  var m = read matrix(int)\n  m.height() + m.width()\n}\n";
    let input = "1 2 3\n4 5 6";
    let (rt, result) = run_main_with_input(src, input);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // 2 rows, 3 cols → 5
    assert_eq!(result.as_int(), 5);
}

/// `matrix` requires a uniform token count, and the fault names the row that
/// broke it.
///
/// The assertion is the span, not merely that a ragged matrix faults: a fault
/// naming the whole input would satisfy the weaker check.
///
/// It is the half `a_ragged_row_fault_names_the_row_in_grid_and_in_matrix`
/// (praxis-runtime's `parser.rs`) cannot cover: it drives the real JIT, so it
/// goes through `ParseDetail::consider`'s deepest-wins filter and gates that the
/// span actually *surfaces* rather than merely that `walk_matrix` returned it.
#[test]
fn m9_matrix_uniformity_faults_on_ragged() {
    let src = "fn main() -> Int {\n  var m = read matrix(int)\n  42\n}\n";
    // Lines are 0..5 (`1 2 3`) and 6..9 (`4 5`). The second is the offender.
    let input = "1 2 3\n4 5";
    let (rt, _result) = run_main_with_input(src, input);
    assert!(
        rt.has_pending_fault(),
        "expected ParseFailed on ragged matrix, got: {:?}",
        rt.fault()
    );
    let detail = rt.parse_detail();
    let fail = detail
        .fail
        .as_ref()
        .expect("a parse failure records its detail (§7.11)");
    assert_eq!(fail.expected, "rectangular matrix row");
    assert_eq!(
        fail.input_span,
        (6, 9),
        "the short row `4 5`, not the whole input"
    );
}

// ===========================================================================
// §7.11 rich parse diagnostics.
//
// A `ParseFailed` fault records structured detail (input span, expected
// description, actual preview) into the runtime's `ParseDetail` slot. These
// tests assert the detail is populated after a parse fault — the foundation the
// noninteractive fallback and the crash REPL's `input`/`parser` commands
// render.
// ===========================================================================

#[test]
fn m10ws1_parse_failed_records_expected_and_preview() {
    // `read lines(int)` against non-integer input faults. The detail should
    // carry a non-empty `expected` and a non-empty `actual_preview`.
    let src = "fn main() -> Int {\n  var xs = read lines(int)\n  0\n}\n";
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
    let src = "fn main() -> Int {\n  var r = read `{a:int}:{b:int}`\n  0\n}\n";
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
    let src = "fn main() -> Int {\n  var xs = read lines(int)\n  xs.len()\n}\n";
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
    let src = "fn main() -> Int {\n  var xs = read lines(int)\n  0\n}\n";
    let input = "line one\nline two\nstill not an int";
    let (rt, _result) = run_main_with_input(src, input);
    assert!(rt.has_pending_fault());
    let preview = &rt.parse_detail().actual_preview;
    assert!(!preview.contains('\n'));
    assert!(!preview.contains('\r'));
}

// ===========================================================================
// Debug-frame codegen wiring (§9.3, ADR-021, ADR-104).
//
// Every generated function claims a debug frame in lockstep with its shadow
// frame — one `DebugFrameEntry` and one value slot per `Gc` local, both
// bump-claimed inline — and writes each local's value into its slot at the
// instruction that defines it. These tests confirm the wiring is balanced and
// non-corrupting: GC rooting stays sound (the run-pass suite guards this), the
// stacks come back empty, and the deepest claim/release sequence — the
// stack-overflow fault path — unwinds cleanly back to the host. The frames'
// *content* (locals at fault time) is made observable by the crash snapshot.
// ===========================================================================

#[test]
fn m10ws2_debug_frame_pushpop_balanced_across_recursion() {
    // Deep recursion claims and releases many debug frames. If the two were
    // unbalanced the stacks would drift upward until the reservation ran out;
    // if a def-store wrote outside its own run it would corrupt a caller's
    // frame. A clean result plus two empty stacks confirms the wiring.
    let src = "fn sum(n: Int) -> Int {\n  if n <= 0 { 0 } else { n + sum(n - 1) }\n}\n
               fn main() -> Int { sum(500) }\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // sum(500) = 500*501/2 = 125250.
    assert_eq!(result.as_int(), 125250);
    assert!(
        rt.debug_frame_stack().is_empty(),
        "every claim was released"
    );
    assert!(rt.debug_value_stack().is_empty());
    assert!(rt.shadow_stack().is_empty());
}

#[test]
fn m10ws2_debug_frame_unwinds_cleanly_on_stack_overflow() {
    // The stack-overflow fault path is the deepest claim/release sequence:
    // every recursed frame has claimed shadow slots, debug value slots and a
    // frame entry. Each fault epilogue must give all three back as it unwinds
    // to the host. `MAX_RECURSION_DEPTH` frames deep is also where an
    // *unbalanced* epilogue would be loudest, since the reservations are sized
    // for exactly that depth (ADR-101, ADR-104).
    let src = "fn count(n: Int) -> Int { count(n + 1) }\n
               fn main() -> Int { count(0) }\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault());
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::StackOverflow);
    // Every frame's fault epilogue restored the tops its prologue saved. The
    // stacks live on the runtime, which outlives the run.
    assert!(
        rt.debug_frame_stack().is_empty(),
        "every claim was released"
    );
    assert!(rt.debug_value_stack().is_empty());
    assert!(rt.shadow_stack().is_empty());
}

#[test]
fn m10ws2_debug_frame_locals_survive_gc_during_recursion() {
    // A recursive function that allocates on every call forces GC at safepoints
    // while the debug stacks are deep. If a def-store wrote outside its own run
    // the GC (which walks the parallel shadow stack) or the returned value
    // would be wrong. The correct sum confirms the two stay consistent across
    // collections.
    let src = "fn build(n: Int) -> Vec[Int] {\n  if n == 0 { Vec() } else { var v = build(n - 1); v.push(n); v }\n}\n
               fn main() -> Int {\n  var v = build(100);\n  var s = 0;\n  var i = 0;\n  while i < v.len() { s = s + v.get(i); i = i + 1 }\n  s\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // sum 1..=100 = 5050.
    assert_eq!(result.as_int(), 5050);
}

// ===========================================================================
// Crash snapshot + GC rooting (§9.3, §19.10 acceptance).
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
    // (the OOB access is inline); a deeper chain is exercised by the GC test
    // below.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.get(5)\n}\n";
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
        "fn main() -> Int {\n  var xs = Vec()\n  xs.push(11)\n  xs.push(22)\n  xs.get(99)\n}\n";
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
    // Force a collection through the runtime's own root set: the runtime-owned
    // snapshot is an arm of it, so this tests that the snapshot is rooted
    // *automatically* rather than because this test remembered to pass it. If
    // retention is broken, the referenced objects are reclaimed and
    // dereferencing a root would be use-after-free. Collect several times to
    // stress the mark/sweep.
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
// Full static `Type` id + source span threaded into the debug frame (§9.3).
// The per-local `type_id` lets the `p EXPR` evaluator reconstruct `Vec[Int]` /
// record shapes the runtime `descriptor` loses; the per-function source span
// lets the `source` command render the faulting function. These assert both
// survive into the crash snapshot.
// ===========================================================================

#[test]
fn m10b_ws1_snapshot_frame_carries_source_span() {
    // `main` has a real source span; the snapshot's frame for `main` must carry
    // a non-empty `[start, end)` byte range, not the `(0, 0)` default. A
    // deliberately-placed OOB get faults inside `main`, so frame 0 is `main`.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.get(9)\n}\n";
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
    // collections to `VEC`) is threaded through, so the `p EXPR` evaluator can
    // reconstruct `Vec[Int]` element types for type-checking.
    let src = "fn main() -> Int {\n  var n = 42\n  var xs = Vec()\n  xs.push(n)\n  xs.get(9)\n}\n";
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

// ===========================================================================
// ADR-104 — the debugger's view is written once per value.
//
// The backend stores a `Gc` local into its debug slot at the instruction that
// *defines* it, instead of re-writing the whole over-approximate
// `DebugSlots::visible()` set at every GC safepoint and every `CheckFault`.
// The three tests below pin the properties that makes load-bearing, and they
// assert on the `CrashSnapshot` rather than through the REPL so the contract
// holds one layer below `m11_locals_split_users_and_temps_with_types` and
// friends.
//
// Together they are also the case against reconstructing the debugger's view
// from the shadow stack: the first shows a value the exact GC root set has
// deliberately dropped, and the third shows one that was never in a shadow slot
// at any point in the program's execution.
// ===========================================================================

/// The `Vec` a local names in `l`, or `None` if that local has no value.
fn snapshot_local_vec(snap: &praxis_runtime::CrashSnapshot, name: &str) -> Option<Vec<i64>> {
    snap.frames
        .iter()
        .flat_map(|f| &f.locals)
        .find(|l| l.name() == name)
        .and_then(|l| l.value)
        // Through `reference()` — the one door from a debug value back to the
        // heap — so the two absences a slot can hold both answer `None` here:
        // "nothing written" and "written, then collected". A `DebugValue` that
        // names no object has no `Vec` to read, and `as_vec` says so by
        // panicking.
        .and_then(praxis_runtime::DebugValue::reference)
        .map(|r| r.as_vec().iter().map(|e| e.as_int()).collect())
}

#[test]
fn a_local_the_root_set_dropped_is_still_renderable() {
    // ADR-044 decision 1, stated at the snapshot level. `xs` is dead for the
    // collector from its last `push` onward, so `RootSlots::dead` nulls its
    // shadow slot at the very next safepoint — the `Vec()` that makes `ys` —
    // and `a_dead_local_stops_being_reachable_from_its_frame` is the test that
    // insists it does. The debugger must show it anyway: the user is asking what
    // the program's state *is*, not what the collector still needs.
    //
    // `xs`'s definition is the only point that writes its debug slot, and the
    // debug slot is never cleared, so the value stays renderable.
    let src = "fn main() -> Int {\n  var xs = Vec()\n  xs.push(11)\n  var ys = Vec()\n  \
               ys.push(22)\n  ys.get(99)\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault());
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::IndexOutOfBounds);
    let snap = rt.crash_snapshot().expect("snapshot captured");
    assert_eq!(
        snapshot_local_vec(snap, "xs"),
        Some(vec![11]),
        "`xs` is dead for GC at the fault and must still be renderable"
    );
    assert_eq!(
        snapshot_local_vec(snap, "ys"),
        Some(vec![22]),
        "`ys` is live at the fault"
    );
}

#[test]
fn a_fault_between_a_definition_and_the_next_safepoint_shows_the_value() {
    // `d` is boxed by an `Inst::ConstGc`/`Inst::Alloc`, then read back as a
    // scalar and divided into — and the division faults before any GC safepoint
    // runs. Store-at-def is what puts the value in the debug slot: without a
    // write at the definition, a snapshot taken on the fault path sees
    // `<uninit>` for the `0` divisor in `n / d`.
    let src = "fn main() -> Int {\n  var n = 10\n  var d = 0\n  n / d\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault());
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::DivByZero);
    let snap = rt.crash_snapshot().expect("snapshot captured");
    let locals = &snap.frames[0].locals;
    let value_of = |name: &str| {
        locals
            .iter()
            .find(|l| l.name() == name)
            .and_then(|l| l.value)
            .map(|r| r.as_int())
    };
    assert_eq!(value_of("d"), Some(0), "the divisor must be renderable");
    assert_eq!(value_of("n"), Some(10), "so must the dividend");
}

#[test]
fn a_temp_that_never_reached_a_shadow_slot_is_still_renderable() {
    // The loss class a reconstruction-from-the-shadow-stack design cannot
    // recover, and the reason ADR-104 rejects one outright.
    //
    // `a + b` is materialized into a temp, and that temp is consumed by the
    // overflowing addition before the next GC safepoint. `liveness::block_roots`
    // computes the root set as live-*before* the instruction, so a
    // `Materialize`'s destination is by construction excluded from its own
    // safepoint's root set — "the destination is written after the collection so
    // it is not rooted". The temp is therefore in *no* shadow slot at any point
    // in this program, yet `locals` must show it: `m11_locals_split_users_and_
    // temps_with_types` asserts on its `@ "a + b"` provenance line.
    let src = "fn main() -> Int {\n  var a = 10\n  var b = 20\n  \
               a + b + 9223372036854775807\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault());
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::IntOverflow);
    let snap = rt.crash_snapshot().expect("snapshot captured");
    let sum = snap.frames[0]
        .locals
        .iter()
        .filter(|l| !l.is_user())
        .filter_map(|l| l.value)
        .map(|r| r.as_int())
        .find(|&v| v == 30);
    assert_eq!(
        sum,
        Some(30),
        "the `a + b` temp lives only in a register and the debug frame, and \
         must be renderable from the latter"
    );
}

/// ADR-120 part 2, end to end and in the kind the failure mode names: a
/// `Float` temp whose box the forwarding elided renders its value, and the
/// value is the `f64` and not its bit pattern.
///
/// **`Float` is the one to test rather than `Int`**, because the scalar channel
/// carries `f64::to_bits()` and the slot carries what the channel carries. An
/// `Int` slot round-trips even if every decode were the identity; this one does
/// not. It is also the risky shape — a collector that followed the slot would
/// dereference an f64 bit pattern as a `GcHeader` — so it is the one whose slot
/// the collector must be shown not to follow.
#[test]
fn an_elided_float_box_renders_its_value_and_not_its_bit_pattern() {
    let src = "fn main() -> Int {\n  var a = 2.5\n  var b = 4.0\n  \
               var c = a * b + b\n  1 / 0\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault());
    let snap = rt.crash_snapshot().expect("snapshot captured");
    let found = snap.frames[0]
        .locals
        .iter()
        .filter_map(|l| l.value)
        .any(|v| v == praxis_runtime::DebugValue::Scalar(praxis_runtime::ScalarValue::Float(10.0)));
    assert!(
        found,
        "`a * b`'s box was forwarded away and its slot must hold 10.0: {:?}",
        snap.frames[0]
            .locals
            .iter()
            .map(|l| l.value)
            .collect::<Vec<_>>()
    );
}

/// The other half of the same program, and the one that is about memory safety
/// rather than fidelity: a snapshot **is** a strong root set (ADR-033), so a
/// scalar slot must not appear in it.
///
/// A `Float` payload is an arbitrary 64-bit word; tracing one would hand the
/// collector a `GcHeader` address it invented. `DebugValue::reference` is where
/// it drops out, and this is the test that says the drop happens.
#[test]
fn an_elided_boxs_scalar_is_not_a_root_of_the_snapshot() {
    use praxis_runtime::RootSet;
    let src = "fn main() -> Int {\n  var a = 2.5\n  var b = 4.0\n  \
               var c = a * b + b\n  1 / 0\n}\n";
    let (rt, _result) = run_main(src);
    let snap = rt.crash_snapshot().expect("snapshot captured");
    let scalars = snap.frames[0]
        .locals
        .iter()
        .filter(|l| matches!(l.value, Some(praxis_runtime::DebugValue::Scalar(_))))
        .count();
    assert!(
        scalars > 0,
        "the program has forwarded boxes to speak about"
    );
    let mut roots = Vec::new();
    snap.push_roots(&mut roots);
    assert_eq!(
        roots.len(),
        snap.frames
            .iter()
            .flat_map(|f| &f.locals)
            .filter(|l| matches!(l.value, Some(praxis_runtime::DebugValue::Reference(_))))
            .count(),
        "every root is a reference and every reference is a root"
    );
}

/// The store lands **after** the fault check, so a temp whose expression
/// overflowed still renders `<uninit>` — the honest answer for a value that was
/// never produced.
///
/// This is ADR-117's fold doing work it was not built for. `store_debug_defs`
/// runs once per lowering *step*, and a checked `IntBinOp` and its
/// `Inst::CheckFault` are one step whose raise block leaves for the fault
/// epilogue; so the overflowing path diverts before the store, exactly as it
/// diverted before the box. At `RaiseExit::Observed`, where the raise
/// converges, this would render the wrapped value instead — which is why the
/// assertion is here and not in a comment.
#[test]
fn an_overflowing_temp_is_not_given_the_wrapped_value_it_never_produced() {
    let src = "fn main() -> Int {\n  var a = 10\n  var b = 20\n  \
               a + b + 9223372036854775807\n}\n";
    let (rt, _result) = run_main(src);
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::IntOverflow);
    let snap = rt.crash_snapshot().expect("snapshot captured");
    let wrapped = 30_i64.wrapping_add(i64::MAX);
    let lied = snap.frames[0]
        .locals
        .iter()
        .filter_map(|l| l.value)
        .any(|v| {
            v == praxis_runtime::DebugValue::Scalar(praxis_runtime::ScalarValue::Int(wrapped))
        });
    assert!(
        !lied,
        "the overflowing sum was never produced; no slot may claim it did"
    );
    // And the control, so this does not pass by the program having no slots:
    // `a + b` did compute, and its slot says so.
    let sum = snap.frames[0]
        .locals
        .iter()
        .filter_map(|l| l.value)
        .any(|v| v == praxis_runtime::DebugValue::Scalar(praxis_runtime::ScalarValue::Int(30)));
    assert!(sum, "`a + b` is 30 and it did produce");
}

#[test]
fn a_snapshot_orders_its_frames_innermost_first_with_each_functions_own_locals() {
    // The debug frames are a *stack*, not a `parent`-linked chain, so the
    // innermost-first order the host renders (`#0` is the faulting function)
    // comes from walking `[base, top)` backwards rather than from following
    // pointers. This is the test for that reversal, and for the rejoin: each
    // frame's locals come from *its* function's static metadata zipped with
    // *its own* run of value slots, so two frames must not show each other's.
    let src = "fn inner(a: Int) -> Int {\n  var deep = a + 1\n  deep / 0\n}\n\
               fn main() -> Int {\n  var outer = 7\n  inner(outer)\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault());
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::DivByZero);
    let snap = rt.crash_snapshot().expect("snapshot captured");
    assert_eq!(snap.len(), 2, "the fault is two frames deep");
    // SAFETY: function names are compiler-embedded 'static UTF-8.
    assert_eq!(unsafe { snap.frame_name(0) }, "inner");
    // SAFETY: as above.
    assert_eq!(unsafe { snap.frame_name(1) }, "main");
    assert_eq!(snap.frames[0].parent, 1, "frame 0's caller is frame 1");
    assert_eq!(snap.frames[1].parent, usize::MAX, "`main` has no caller");

    let named = |i: usize, name: &str| {
        snap.frames[i]
            .locals
            .iter()
            .find(|l| l.name() == name)
            .and_then(|l| l.value)
            .map(|r| r.as_int())
    };
    assert_eq!(named(0, "a"), Some(7), "the callee's parameter");
    assert_eq!(named(0, "deep"), Some(8), "the callee's own local");
    assert_eq!(named(1, "outer"), Some(7), "the caller's local");
    assert_eq!(named(1, "deep"), None, "`deep` belongs to `inner` alone");
    assert_eq!(named(0, "outer"), None, "`outer` belongs to `main` alone");

    // Each frame's span is its own function's.
    let (s0, e0) = snap.frames[0].source_span;
    let (s1, e1) = snap.frames[1].source_span;
    assert!(src[s0 as usize..e0 as usize].starts_with("fn inner"));
    assert!(src[s1 as usize..e1 as usize].starts_with("fn main"));
}

// ===========================================================================
// ADR-106 — the debug values are the collector's one weak arm.
//
// The three tests above say a value the root set dropped stays renderable, and
// they are true because nothing ever clears a debug slot. This one says what
// happens when the collector actually *runs* in that window: the value the
// debugger names has become garbage, and its storage is reusable.
//
// Both tests below and the three above must pass together. That pairing is the
// whole content of the decision — the arm keeps the slots valid without keeping
// them alive — and `a_dead_local_stops_being_reachable_from_its_frame` in
// `adversarial_audit.rs` is the third leg, saying the arm stayed weak.
// ===========================================================================

/// ADR-106 in a real compiled program.
///
/// `xs` is dead for the collector from its last read onward, so `RootSlots::dead`
/// nulls its shadow slot while its debug slot keeps naming the `Vec`. The loop
/// after it allocates well past `INITIAL_COLLECT_THRESHOLD`, so a paced
/// collection runs *inside that window* and reclaims `xs` — and then keeps
/// allocating, so the block comes back as something else.
///
/// Without the weak arm the snapshot would copy that reference and the debugger
/// would read a reissued block through `xs`'s static `Vec` descriptor. With it,
/// `xs` is an absence: `snapshot_local_vec` finds the local and finds no value,
/// which is the `<uninit>` decision 4 chose.
///
/// The `+ 2000` offsets every element past the interned small-`Int` range, for
/// the reason `a_dead_local_stops_being_reachable_from_its_frame` gives: an
/// interned `Int` is an immortal no sweep touches, and this test needs real
/// allocations to die.
#[test]
fn a_local_the_collector_reclaimed_renders_as_an_absence_not_as_a_dangling_ref() {
    let src = "\
fn main() -> Int {
  var xs = Vec()
  var i = 0
  while i < 200 {
    xs.push(i + 2000)
    i = i + 1
  }
  var sum = xs.len()
  var j = 0
  while j < 40000 {
    var junk = Vec()
    junk.push(j + 2000)
    sum = sum + junk.len()
    j = j + 1
  }
  var ys = Vec()
  ys.push(sum)
  ys.get(99)
}
";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault());
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::IndexOutOfBounds);
    let snap = rt.crash_snapshot().expect("snapshot captured");
    assert_eq!(
        snapshot_local_vec(snap, "ys"),
        Some(vec![40_200]),
        "`ys` is live at the fault and must be renderable — if this is None the \
         weak scan is nulling live values and the predicate is wrong"
    );
    assert_eq!(
        snapshot_local_vec(snap, "xs"),
        None,
        "`xs` was reclaimed by a collection inside the window between its last \
         use and the fault; the snapshot must copy an absence rather than a \
         reference into storage the allocator has since handed back out"
    );
    // And it is the *collected* absence, not the unwritten one — the slot says
    // which, so the debugger can render `<collected>` rather than `<uninit>` for
    // a `var` the program plainly ran.
    let xs = snap
        .frames
        .iter()
        .flat_map(|f| &f.locals)
        .find(|l| l.name() == "xs")
        .expect("`xs` is a local of the faulting frame");
    assert_eq!(
        xs.value,
        Some(praxis_runtime::DebugValue::Reclaimed),
        "a local the collector took is distinguishable from one never written"
    );
}

/// The counterexample that keeps the test above honest: the *same* program with
/// `xs` read after the allocating loop keeps it renderable with its real
/// contents.
///
/// Without this, nulling every debug slot at every collection would pass the
/// test above and destroy the debugger, and
/// `a_local_the_root_set_dropped_is_still_renderable` would not catch it —
/// that program never collects at all.
#[test]
fn a_local_that_survives_the_collection_is_renderable_with_its_real_contents() {
    let src = "\
fn main() -> Int {
  var xs = Vec()
  var i = 0
  while i < 3 {
    xs.push(i + 2000)
    i = i + 1
  }
  var sum = 0
  var j = 0
  while j < 40000 {
    var junk = Vec()
    junk.push(j + 2000)
    sum = sum + junk.len()
    j = j + 1
  }
  var ys = Vec()
  ys.push(sum + xs.len())
  ys.get(99)
}
";
    let (rt, _result) = run_main(src);
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::IndexOutOfBounds);
    let snap = rt.crash_snapshot().expect("snapshot captured");
    assert_eq!(
        snapshot_local_vec(snap, "xs"),
        Some(vec![2000, 2001, 2002]),
        "`xs` is live across every collection in this program — the weak scan \
         must leave it alone, elements and all"
    );
}

/// `panic` raises its own fault kind, carries the words the program wrote, and
/// stops the program where it stands.
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
        "fn main() -> Int {\n  var xs = Vec()\n  xs.push(1)\n  dbg(xs).push(2)\n  xs.len()\n}\n",
    );
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 2);
}

/// Each of the seven numeric helpers computes the number it names.
///
/// Inference can only say the type is `Int` — that the wrapper behind the name
/// is the *right* wrapper is a fact only a run can establish, and a table that
/// named the wrong symbol would typecheck identically (which is why
/// `each_helper_has_its_own_wrapper` exists too).
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
/// wrapping, which is the same answer `+`/`-`/`*` give (§4.12): a number nobody
/// wrote is worse than a stop. `sign`, `min` and `max` are total and are here to
/// show the fault is not blanket caution.
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

    // An inverted `clamp` range is empty, so there is no value to return; the
    // fault kind is its own (`EmptyRange`, ADR-075).
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

// --- §6.5's graph helpers (ADR-060) -----------------------------------------

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

/// The two traversals visit every reachable state, once, in the order their
/// names promise.
///
/// The half no type test can see. Inference says the result is `Vec[Int]`; that
/// the wrapper behind the name walks the graph *at all* — that it calls the
/// closure, reads the `Vec` it hands back, and recognizes a state it has already
/// seen — is a fact only a run establishes.
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
         \x20 var seen = flood_fill(1, |n| steps(n))\n\
         \x20 if seen.contains(4) {{ 1 }} else {{ 0 }}\n\
         }}"
    ));
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
    let (rt, result) = run_main(&format!(
        "{DIAMOND}fn main() -> Int {{\n\
         \x20 var seen = flood_fill(1, |n| steps(n))\n\
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
         \x20 var costs = dijkstra(P {{ x: 0, y: 0 }}, |p| steps(p), |a, b| 1)\n\
         \x20 costs[P {{ x: 2, y: 2 }}]\n\
         }}"
    ));
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 4);
}

/// A walk holds every state it has seen in Rust structures the collector cannot
/// scan, so each one has to be rooted in the native frame. A graph big enough to
/// collect several times over, whose neighbour function allocates on every call,
/// is what makes the rooting observable: without it the visited set holds
/// reclaimed objects and the answer is wrong or the host dies.
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
         \x20 var d = bfs_distance(1, |n| steps(n), |n| panic(\"stop\"))\n\
         \x20 0\n\
         }",
    );
    assert!(rt.has_pending_fault());
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::Panic);
}

/// ADR-059: a `for` over a range runs the iterations the range names. This is
/// the half no type test can see — the loop reads `praxis_range_len` and
/// `praxis_range_get`, so a wrong `len`/`get` symbol selection typechecks
/// identically and then iterates the wrong collection.
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

    // A descending range is empty, not a countdown — the body never runs.
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

/// A range is a **value**, not only a `for`-header form: it survives being
/// bound to a name, passed through a function, stored in a collection and used as
/// a `Map` key — and it renders as the half-open interval it is.
#[test]
fn a_range_is_a_value_that_outlives_its_expression() {
    // Bound to a `var`, then iterated — the range object has to survive the
    // binding and the allocations between.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var r = 2..6\n  var v = Vec()\n  v.push(9)\n\
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
        run_main("fn main() -> Int {\n  var m = Map()\n  m.insert(0..3, 41)\n  m[0..3] + 1\n}\n");
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 42);

    // …and `1..=4` really is `1..5`, which is what normalizing at construction
    // means: the two spellings are one key and one rendering.
    let (rt, result) =
        run_main("fn main() -> Int {\n  var m = Map()\n  m.insert(1..=4, 5)\n  m[1..5]\n}\n");
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 5);
}

/// §3.3's representative program computes `sign` and `abs` on values it read,
/// and `max(abs(dx), abs(dy))` over them. A helper has to survive being nested
/// in an expression, called with computed operands, and used as another
/// function's argument.
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

/// The §5.2 program the design doc promises needs no annotations. `total`'s
/// parameter has no type until `main` calls it, and the `.sum()` inside it has
/// no catalog entry when inference walks past it — the entry is selected later,
/// when the receiver resolves, and this is where that shows: the method has to
/// *lower*, which is the half a type test cannot see.
#[test]
fn a_method_on_an_unannotated_parameter_runs() {
    let (rt, result) = run_main(
        "fn total(values) { values.sum() }\n\
         fn main() -> Int {\n  \
           var values = Vec()\n  \
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
           var values = Vec()\n  \
           values.push(1)\n  \
           add(values, 41)\n  \
           values.sum()\n\
         }\n",
    );
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 42);
}

// ---- Function values (ADR-061) ----

/// A top-level `fn` used as a value is a callable closure.
///
/// `praxis check` accepts such a program either way, because a `fn`'s type *is*
/// a `Func`, so a value that lowered to `Unit` would be read as a function
/// pointer and take the host down: **this test aborting the test process is the
/// failure mode**, not a wrong answer.
///
/// Three routes are here: a `var`, a parameter of declared function type, and a
/// graph helper's closure argument. A `Vec` element is a fourth, and is the one
/// that also exercises the postfix call form.
#[test]
fn a_top_level_fn_is_a_callable_value() {
    // Through a `var`, then called by name.
    let (rt, result) = run_main(
        "fn double(n: Int) -> Int { n * 2 }\n\
         fn main() -> Int {\n  var f = double\n  f(3)\n}\n",
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
         fn main() -> Int {\n  var f = sub\n  f(50, 8)\n}\n",
    );
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 42);

    // Stored in a collection and called postfix, which reaches the value
    // through `callee_expr` rather than through a name.
    let (rt, result) = run_main(
        "fn double(n: Int) -> Int { n * 2 }\n\
         fn main() -> Int {\n  var fs = Vec()\n  fs.push(double)\n  fs.get(0)(21)\n}\n",
    );
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 42);
}

/// …and through a graph helper, where `praxis-runtime` calls *back* into
/// generated code. The helper receives the adapter's closure and calls it once
/// per state; a `Unit` here would fail its descriptor check as a `TypeMismatch`
/// fault.
#[test]
fn a_fn_value_is_callable_from_the_runtime_side() {
    let (rt, result) = run_main(
        "fn step(n: Int) -> Vec[Int] {\n  \
           var v = Vec()\n  \
           if n < 4 { v.push(n + 1) }\n  \
           v\n\
         }\n\
         fn at_goal(n: Int) -> Bool { n == 4 }\n\
         fn main() -> Int {\n  \
           var d = bfs_distance(0, step, at_goal)\n  \
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
         fn main() -> Int {\n  var f = half\n  f(10)\n}\n",
    );
    assert!(rt.has_pending_fault(), "the DivByZero has to arrive");
}

/// A `for` over an unannotated parameter runs, and runs against the iterable
/// each call site chose.
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
/// The **element** is the other half: the loop variable's type is the
/// collection's *element* type, not the collection's — recording the collection
/// makes `copy` infer `Vec[Vec[Int]]` and fault with "value does not have the
/// declared type" out of a program `praxis check` accepts.
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
    // *elements*.
    const COPY: &str = "fn copy(vs) { var o = Vec()\n for v in vs { o.push(v) }\n o }\n";
    let (rt, result) = run_main(&format!(
        "{COPY}fn main() -> Int {{ var s = Vec()\n s.push(7)\n s.push(9)\n \
         var d = copy(s)\n d.get(0) + d.get(1) }}"
    ));
    assert!(!rt.has_pending_fault(), "copy over a Vec faulted");
    assert_eq!(result.as_int(), 16);
    // …and the same body over a different ctor, so the element type is read from
    // the argument and not from the first use.
    let (rt, result) = run_main(&format!(
        "{COPY}fn main() -> Int {{ var d = copy(1..4)\n d.len() + d.get(2) }}"
    ));
    assert!(!rt.has_pending_fault(), "copy over a Range faulted");
    assert_eq!(result.as_int(), 6);
}

/// `&&` and `||` short-circuit: the right operand is not evaluated on the path
/// that cannot need it.
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
           var diagonals = false\n  var dx = 1\n  var dy = 0\n  \
           if !diagonals && dx != 0 && dy != 0 { 9 } else { 8 }\n\
         }",
    );
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 8);
}

/// A tuple element read at run time is the element the position names, at every
/// arity and through every shape that holds one.
///
/// The half no type test can see: a tuple index needs an instruction of its own,
/// reaching `praxis_tuple_get`. `Inst::LoadField` hard-codes
/// `praxis_record_field`, so a `LoadField` reused here would read a record's
/// field table out of a tuple's payload.
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
        ("fn main() -> Int { var p = (7, 9)\n p.0 + p.1 }", 16),
        (
            "fn snd(p: (Int, Int)) -> Int { p.1 }\nfn main() -> Int { snd((3, 4)) }",
            4,
        ),
        (
            "fn main() -> Int { var f = |p: (Int, Int)| p.0 * p.1\n f((6, 7)) }",
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

/// A subscript reads and writes the collection it names, through every receiver
/// that has the operation.
///
/// The half no type test can see: which runtime wrapper each row selects. A
/// `Counter` read that reached `praxis_map_get` would answer Unit where §6.2
/// promises zero, and a `Grid` store that forgot to pass both coordinates would
/// write the wrong cell — both type-check.
#[test]
fn a_subscript_reads_and_writes_through_the_wrapper_its_receiver_needs() {
    // Read, on each of the six.
    for (src, want) in [
        ("var v = Vec()\n v.push(10)\n v.push(20)\n v[1]", 20),
        (
            "var d = Deque()\n d.push_back(4)\n d.push_front(9)\n d[0]",
            9,
        ),
        ("var m = Map()\n m.insert(\"a\", 7)\n m[\"a\"]", 7),
        // §6.2: a `Counter`'s absent key reads as zero and does not fault, which
        // is the one read that differs from its `Map` sibling's.
        (
            "var c = Counter()\n c.inc(\"a\")\n c[\"a\"] + c[\"nope\"]",
            1,
        ),
        // `Text`'s read answers a `Char` (ADR-086), so naming its scalar value
        // in an `Int`-returning `main` takes the conversion. The row under test
        // is still the subscript; `.to_int()` is how the answer is spelled.
        ("\"abc\"[1].to_int()", 98),
    ] {
        let (rt, result) = run_main(&format!("fn main() -> Int {{\n  {src}\n}}\n"));
        assert!(!rt.has_pending_fault(), "{src} faulted: {:?}", rt.fault());
        assert_eq!(result.as_int(), want, "{src}");
    }

    // Store, on the three that have one — and read back through the subscript, so
    // the pair has to agree about which collection it is talking to.
    for (src, want) in [
        ("var m = Map()\n m[\"a\"] = 5\n m[\"a\"]", 5),
        ("var m = Map()\n m[\"a\"] = 5\n m[\"a\"] = 6\n m.len()", 1),
        ("var c = Counter()\n c[\"a\"] = 4\n c[\"a\"]", 4),
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
            "fn main() -> Int {{\n  var c = Counter()\n  c[\"k\"] = 10\n  c[\"k\"] {op} 3\n  c[\"k\"]\n}}\n"
        );
        let (rt, result) = run_main(&src);
        assert!(!rt.has_pending_fault(), "{op} faulted: {:?}", rt.fault());
        assert_eq!(result.as_int(), want, "{op}");
    }

    // A `Counter`'s zero default is what makes `counts[key] += 1` work on a key
    // that has never been seen — §3.3 never initializes one.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var c = Counter()\n  c[\"a\"] += 1\n  c[\"a\"] += 1\n  \
         c[\"b\"] += 5\n  c[\"a\"] * 100 + c[\"b\"]\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 205);

    // A tuple key, which is what §3.3 counts by. The key is a fresh allocation
    // every iteration, so this only works if identity is structural (ADR-026).
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var c = Counter()\n  \
         for i in 0..3 { c[(1, 2)] += 1 }\n  c[(1, 2)] * 10 + c.len()\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 31, "one key, counted three times");
}

/// The two-coordinate subscript (§6.4): `grid[x, y]` reads and writes the cell
/// at (x, y), and the coordinate order is x-then-y.
#[test]
fn a_grid_subscript_takes_both_coordinates_in_the_order_the_design_names() {
    // A 2×2 grid read from input (a `Grid()` is 0×0, so it has no cell to name).
    // The written cell is deliberately **off the diagonal**: a store that reached
    // `praxis_grid_set(g, y, x, v)` would pass on a diagonal cell.
    //
    // The subject here is subscript argument order, not grid cell semantics:
    // that a `grid(int)` cell is a whole token is gated by
    // `a_grid_cell_is_whatever_its_cell_parser_reads` (parser.rs) and
    // `a_grid_of_char_is_positional_so_a_space_is_a_cell`
    // (adversarial_audit.rs).
    let (rt, result) = run_main_with_input(
        "fn main() -> Int {\n  var g = read grid(int)\n  g[1, 0] = 7\n  \
         g[1, 0] * 100 + g[0, 0] * 10 + g[0, 1]\n}\n",
        "1 2\n3 4\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 700 + 10 + 3);

    // `.get`/`.set` and the subscript are the same cell, which is what says the
    // two spellings are one operation for a `Grid` (unlike a `Map`'s, §4.7).
    let (rt, result) = run_main_with_input(
        "fn main() -> Int {\n  var g = read grid(int)\n  g.set(1, 1, 5)\n  \
         g[1, 1] * 10 + g.get(1, 1)\n}\n",
        "1 2\n3 4\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 55);

    // Out of range faults rather than reading a neighbour, from either side.
    let (rt, _) = run_main_with_input(
        "fn main() -> Int {\n  var g = read grid(int)\n  g[99, 0]\n}\n",
        "1 2\n3 4\n",
    );
    assert!(rt.has_pending_fault(), "an out-of-range read faults");
    let (rt, _) = run_main_with_input(
        "fn main() -> Int {\n  var g = read grid(int)\n  g[0, 99] = 1\n  0\n}\n",
        "1 2\n3 4\n",
    );
    assert!(rt.has_pending_fault(), "an out-of-range store faults");
}

/// A compound assignment through a subscript evaluates its receiver and indices
/// **once**.
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
         fn main() -> Int {\n  var log = Vec()\n  var c = Counter()\n  \
         c[key(log)] += 1\n  log.len() * 10 + c[\"k\"]\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 11, "one call to `key`, one increment");

    // …and the receiver too, which is the other half a desugaring would double.
    let (rt, result) = run_main(
        "fn pick(log, c) { log.push(1)\n c }\n\
         fn main() -> Int {\n  var log = Vec()\n  var c = Counter()\n  \
         pick(log, c)[\"k\"] += 2\n  log.len() * 10 + c[\"k\"]\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 12, "one call to `pick`, one increment");
}

/// **A `Vec`/`Deque` element store.** `v[0] = 100` writes the slot `v[0]` reads.
///
/// The assertions are the ones a plausible-but-wrong wrapper would fail. A store
/// that reached for `praxis_vec_push` would *append*, so the length is checked
/// alongside the element; an index the vector does not hold must fault rather
/// than grow it, which is the same wrong answer from the other side; and the
/// element descriptor has to be reconciled, or a `Vec[Int]` retagged by one bad
/// store would read every remaining payload through the new type.
#[test]
fn a_sequence_stores_the_element_its_subscript_reads() {
    // Replace, not append: the length is unchanged and the neighbours are not.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var v = [1, 2, 3]\n  v[0] = 100\n  \
         v[0] * 1000 + v[1] * 100 + v[2] * 10 + v.len()\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 100_000 + 200 + 30 + 3);

    // The compound form reads through the same subscript and writes back once.
    let (rt, result) =
        run_main("fn main() -> Int {\n  var v = [1, 2, 3]\n  v[2] += 10\n  v[2]\n}\n");
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 13);

    // A `Deque`, indexed from the front.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var d = Deque[Int]()\n  d.push_back(1)\n  \
         d.push_back(2)\n  d[1] = 7\n  d[0] * 10 + d[1]\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 17);

    // One past the end is out of range and **not** a push: this is the assertion
    // that fails if the store is ever pointed at the appending wrapper.
    for src in [
        "var v = [1, 2, 3]\n  v[3] = 9\n  v.len()",
        "var d = Deque[Int]()\n  d.push_back(1)\n  d[1] = 9\n  d.len()",
    ] {
        let (rt, _) = run_main(&format!("fn main() -> Int {{\n  {src}\n}}\n"));
        assert!(
            rt.has_pending_fault(),
            "{src}: a store past the end must fault rather than append"
        );
    }
    // …and so is a negative index, which `as usize` alone would wrap.
    let (rt, _) = run_main("fn main() -> Int {\n  var v = [1]\n  v[0 - 1] = 9\n  v.len()\n}\n");
    assert!(rt.has_pending_fault(), "a negative index faults");
}

/// **A record field store.** `p.x = 5` writes the slot `p.x` reads (§4.5).
///
/// What a type test cannot see: the slot **index**. A store that derived its own
/// index instead of reading the record definition would type-check and then
/// write the wrong field, so every assertion here reads a *different* field back
/// than the one it wrote.
#[test]
fn a_field_store_writes_the_slot_the_field_read_reads() {
    // The written field changes and its neighbour does not — which is the half a
    // wrong index gets backwards.
    let (rt, result) = run_main(
        "struct P { x: Int, y: Int }\n\
         fn main() -> Int {\n  var p = P { x: 1, y: 2 }\n  p.x = 5\n  \
         p.x * 10 + p.y\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 52);

    // The compound forms read the same slot they write.
    let (rt, result) = run_main(
        "struct P { x: Int, y: Int }\n\
         fn main() -> Int {\n  var p = P { x: 1, y: 2 }\n  p.y += 10\n  p.y *= 2\n  \
         p.x * 100 + p.y\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 124);

    // The write is on the **object**, so it is visible through every reference to
    // it — §4.2's "passing an argument copies its `GcRef`". A store that rebuilt
    // the record instead would leave the caller's copy at 1.
    let (rt, result) = run_main(
        "struct P { x: Int }\n\
         fn bump(q) { q.x += 1 }\n\
         fn main() -> Int {\n  var p = P { x: 1 }\n  bump(p)\n  bump(p)\n  p.x\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);

    // A nested place: the receiver of a field store is any expression, so
    // `o.inner.v` and `rs[0].v` reach the record the outer one holds.
    let (rt, result) = run_main(
        "struct I { v: Int }\nstruct O { inner: I }\n\
         fn main() -> Int {\n  var o = O { inner: I { v: 1 } }\n  o.inner.v = 4\n  \
         var rs = [I { v: 1 }, I { v: 2 }]\n  rs[0].v = 9\n  \
         o.inner.v * 100 + rs[0].v * 10 + rs[1].v\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 492);

    // A `Text` field, whose `+=` is concatenation (ADR-085) rather than the
    // arithmetic the other four compounds take.
    let (rt, result) = run_main(
        "struct N { name: Text }\n\
         fn main() -> Int {\n  var n = N { name: \"ab\" }\n  n.name += \"cd\"\n  \
         n.name.len()\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 4);
}

/// **The field store's evaluation rule**, which is `IndexAssign`'s: a compound
/// assignment through a field evaluates its receiver **once**.
///
/// `p.x += 1` desugared to `p.x = p.x + 1` names the receiver twice, and MIR
/// lowers each `TypedExpr` where it stands — so `pick(log).x += 1` would call
/// `pick` twice. `TypedStmt::FieldAssign` carries the receiver once, and this is
/// the test that fails if it is ever desugared.
#[test]
fn a_compound_assignment_through_a_field_evaluates_its_receiver_once() {
    let (rt, result) = run_main(
        "struct C { n: Int }\n\
         fn pick(log, c) { log.push(1)\n c }\n\
         fn main() -> Int {\n  var log = Vec()\n  var c = C { n: 0 }\n  \
         pick(log, c).n += 5\n  log.len() * 10 + c.n\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 15, "one call to `pick`, one increment");
}

/// A constructor with written type arguments runs, and it builds the collection
/// the annotation names.
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
        "fn main() -> Int {\n  var c = Counter[(Int, Int)]()\n  \
         for i in 0..3 { c[(1, 2)] += 1 }\n  c[(1, 2)] * 10 + c.len()\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 31);

    // Each ctor arity, and a nested argument.
    for (src, want) in [
        ("var v = Vec[Int]()\n  v.push(4)\n  v[0]", 4),
        ("var m = Map[Text, Int]()\n  m[\"a\"] = 9\n  m[\"a\"]", 9),
        (
            "var m = Map[Text, Vec[Int]]()\n  var inner = Vec()\n  inner.push(3)\n  \
             m[\"a\"] = inner\n  m[\"a\"].len()",
            1,
        ),
    ] {
        let (rt, result) = run_main(&format!("fn main() -> Int {{\n  {src}\n}}\n"));
        assert!(!rt.has_pending_fault(), "{src} faulted: {:?}", rt.fault());
        assert_eq!(result.as_int(), want, "{src}");
    }
}

/// A keyed collection can be enumerated, in a deterministic order, and `count`
/// takes a predicate.
///
/// §3.3's representative program ends `counts.values().count(|n| n >= 2)`. The
/// order is asserted because a `HashMap`'s own iteration order is randomized per
/// process — without a fixed order the *answer* of a program like `m.keys()[0]`
/// would change between runs, not merely its rendering.
#[test]
fn a_keyed_collection_enumerates_in_a_deterministic_order() {
    // `values()` on a Counter, which is what §3.3 needs.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var c = Counter[Text]()\n  c[\"a\"] = 3\n  c[\"b\"] = 1\n  \
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
            "fn main() -> Int {{\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  \
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
        "fn main() -> Int {\n  var m = Map[Text, Int]()\n  m[\"c\"] = 3\n  m[\"a\"] = 1\n  \
         m[\"b\"] = 2\n  var ks = m.keys()\n  var vs = m.values()\n  \
         var ok = 0\n  for i in 0..ks.len() { if m[ks[i]] == vs[i] { ok += 1 } }\n  ok\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(
        result.as_int(),
        3,
        "every index pairs a key with its own value"
    );

    // The order itself, twice in one process and asserted against the key order
    // the formatter already uses — the key's own `compare`, not its rendering
    // (ADR-138).
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var m = Map[Text, Int]()\n  m[\"c\"] = 3\n  m[\"a\"] = 1\n  \
         m[\"b\"] = 2\n  var vs = m.values()\n  vs[0] * 100 + vs[1] * 10 + vs[2]\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 123, "ordered by key: a, b, c");

    // An empty collection enumerates to an empty `Vec` rather than faulting.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var m = Map[Text, Int]()\n  \
         m.keys().len() + m.values().len()\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

/// A template literal that begins with a space matches.
///
/// The interpreter honours a literal's whitespace policy *before* matching its
/// bytes, so the scanner strips the leading space run out of the literal's text:
/// left in both places the space is consumed twice and the literal can never
/// match. §3.3's own template is
/// `` `{x1:int},{y1:int} -> {x2:int},{y2:int}` ``.
#[test]
fn a_template_literal_that_begins_with_a_space_matches() {
    // §3.3's own shape.
    let (rt, result) = run_main_with_input(
        "fn main() -> Int {\n  var rs = read lines(`{a:int} -> {b:int}`)\n  \
         var t = 0\n  for r in rs { t = t + r.a * r.b }\n  t\n}\n",
        "1 -> 2\n3 -> 4\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 14);

    let (rt, result) = run_main_with_input(
        "fn main() -> Int {\n  \
         var rs = read lines(`{x1:int},{y1:int} -> {x2:int},{y2:int}`)\n  \
         var t = 0\n  for r in rs { t = t + r.x2 - r.x1 + r.y2 - r.y1 }\n  t\n}\n",
        "0,9 -> 5,9\n8,0 -> 0,8\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    // (5 - 0) + (9 - 9) for the first line, (0 - 8) + (8 - 0) for the second.
    assert_eq!(result.as_int(), 5);

    // The policy is still flexible, which is what stripping the run *into* it
    // preserves: one space or many, tabs or spaces, all match.
    //
    // `SpaceRun` means "one or more spaces or tabs". The scanner tags a literal
    // with no run in front of it `WsPolicy::None`, so the policy can require one
    // without breaking `{a:int},{b:int}` — and `"1->2"` against a template that
    // wrote ` -> ` is a mismatch, asserted just below.
    for input in ["1 -> 2\n", "1    ->    2\n", "1\t->\t2\n"] {
        let (rt, result) = run_main_with_input(
            "fn main() -> Int {\n  var rs = read lines(`{a:int} -> {b:int}`)\n  \
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

    // And the other side of "one or more": a run the template asked for has to
    // be there.
    let (rt, _result) = run_main_with_input(
        "fn main() -> Int {\n  var rs = read lines(`{a:int} -> {b:int}`)\n  rs.len()\n}\n",
        "1->2\n",
    );
    assert_eq!(
        rt.fault(),
        praxis_runtime::FaultKind::ParseFailed,
        "a template that wrote a space run does not match input that has none"
    );

    // A literal with no leading space is untouched, and one that is *only* spaces
    // is a whitespace part.
    let (rt, result) = run_main_with_input(
        "fn main() -> Int {\n  var rs = read lines(`{a:int},{b:int}`)\n  \
         var t = 0\n  for r in rs { t = t + r.a + r.b }\n  t\n}\n",
        "1,2\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
    let (rt, result) = run_main_with_input(
        "fn main() -> Int {\n  var rs = read lines(`{a:int} {b:int}`)\n  \
         var t = 0\n  for r in rs { t = t + r.a + r.b }\n  t\n}\n",
        "1 2\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

/// A space/tab run at the *end* of a template literal is the whitespace policy
/// too — required, and consumed by the policy rather than by the capture that
/// follows.
///
/// `flush` emits a policy for the trailing run as well as the leading one. A
/// trailing run that belonged to nobody would be neither required
/// (`` `x: {a:rest}` `` would match `x:hello`) nor consumed (`a` would be
/// `" hello"`, with the space the template wrote) — a silent wrong answer.
///
/// A capture is offered the bytes at the cursor, whitespace and all, and
/// `walk_atomic` decides. So `rest`, `text` and `char` keep a space the policy
/// did not take.
#[test]
fn a_template_literals_trailing_space_run_is_its_policy_too() {
    // (1) The run is CONSUMED by the policy, not by the capture: `a` is the
    // eleven bytes after the space, not twelve starting with it.
    let (rt, result) = run_main_with_input(
        "fn main() -> Int {\n  var rs = read lines(`x: {a:rest}`)\n  \
         var t = 0\n  for r in rs { t = t + r.a.len() }\n  t\n}\n",
        "x: hello world\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(
        result.as_int(),
        11,
        "the space after `x:` is the literal's policy, so `rest` starts at `h`"
    );

    // The same shape, asserting the bytes rather than their count — an AoC
    // `Card {id:int}: {body:rest}` whose body is compared against what the file
    // actually holds.
    let (rt, result) = run_main_with_input(
        "fn main() -> Int {\n  var rs = read lines(`Card {id:int}: {body:rest}`)\n  \
         var t = 0\n  for r in rs { if r.body == \"41 48 83\" { t = t + r.id } }\n  t\n}\n",
        "Card 1: 41 48 83\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(
        result.as_int(),
        1,
        "`body` is the text after `: `, so it equals the bytes the file wrote"
    );

    // `text` bounded by a following literal, same rule.
    let (rt, result) = run_main_with_input(
        "fn main() -> Int {\n  var rs = read lines(`x: {a:text} END`)\n  \
         var t = 0\n  for r in rs { t = t + r.a.len() }\n  t\n}\n",
        "x: hello END\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 5, "`a` is `hello`, not `\" hello\"`");

    // (2) The run is REQUIRED — §7.2's ordinary run "matches one or more spaces
    // or tabs", and that is the same one-or-more the leading half enforces.
    let (rt, _result) = run_main_with_input(
        "fn main() -> Int {\n  var rs = read lines(`x: {a:rest}`)\n  rs.len()\n}\n",
        "x:hello\n",
    );
    assert_eq!(
        rt.fault(),
        praxis_runtime::FaultKind::ParseFailed,
        "a template that wrote a trailing run does not match input that has none"
    );

    // The mirror of the leading-run assertion in the test above: `-> ` and
    // ` ->` are the same policy on opposite sides, and both refuse `1->2`.
    let (rt, _result) = run_main_with_input(
        "fn main() -> Int {\n  var rs = read lines(`{a:int}-> {b:int}`)\n  rs.len()\n}\n",
        "1->2\n",
    );
    assert_eq!(
        rt.fault(),
        praxis_runtime::FaultKind::ParseFailed,
        "the trailing spelling refuses `1->2` exactly as the leading one does"
    );
    // …and matches when the run is there, flexibly.
    for input in ["1-> 2\n", "1->    2\n", "1->\t2\n"] {
        let (rt, result) = run_main_with_input(
            "fn main() -> Int {\n  var rs = read lines(`{a:int}-> {b:int}`)\n  \
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

    // (3) A `choice` behind a literal with a trailing run. If the space stayed
    // in the literal's text the capture would be offered it, and the
    // alternative's own literal `plus` would be looked for at the space.
    let (rt, result) = run_main_with_input(
        "fn main() -> Int {\n  \
         var rs = read lines(`op: {g:choice(Plus: `plus {n:int}`, Times: `times {n:int}`)}`)\n  \
         rs.len()\n}\n",
        "op: plus 3\nop: times 4\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);

    // A nested template body behind the same literal, and `char`, which reads
    // the space if it is handed one.
    let (rt, result) = run_main_with_input(
        "fn main() -> Int {\n  var rs = read lines(`op: {g:`plus {n:int}`}`)\n  \
         var t = 0\n  for r in rs { t = t + r.g.n }\n  t\n}\n",
        "op: plus 3\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);

    let (rt, result) = run_main_with_input(
        "fn main() -> Int {\n  var rs = read lines(`x: {a:char}`)\n  rs.len()\n}\n",
        "x: A\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1, "`char` is handed `A`, not `\" A\"`");

    // (4) The interaction with the rule that *whitespace the parser was offered
    // and did not read is nobody's*. A trailing run that is POLICY is required and
    // consumed; a trailing run that is INPUT, past the last part, is still
    // forgiven by `walk_exact`. `{a:int} ` has one policy part, not two, and it
    // is satisfied by the line's own trailing space.
    let (rt, result) = run_main_with_input(
        "fn main() -> Int {\n  var rs = read lines(`{a:int},{b:int}`)\n  \
         var t = 0\n  for r in rs { t = t + r.a + r.b }\n  t\n}\n",
        "1,2 \n",
    );
    assert!(
        !rt.has_pending_fault(),
        "trailing input whitespace no part asked for is nobody's: {:?}",
        rt.fault()
    );
    assert_eq!(result.as_int(), 3);

    // A literal that is *only* a run stays one part: it must not be counted as
    // a leading run and a trailing run and demand two.
    let (rt, result) = run_main_with_input(
        "fn main() -> Int {\n  var rs = read lines(`{a:int} {b:int}`)\n  \
         var t = 0\n  for r in rs { t = t + r.a + r.b }\n  t\n}\n",
        "1 2\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
}

// ===========================================================================
// Every iterable has a `for` lowering (ADR-066)
// ===========================================================================

/// `capability::iter_item` says all ten collections are iterable, and each needs
/// its own lowering: MIR's symbol pickers need an arm per collection, or a
/// defaulted `praxis_vec_get` reads a `Set`'s payload as a `Vec`'s.
///
/// So: one `for` per iterable, each answering something a wrong read could not
/// produce. Two of the failure modes are not assertions — **hanging and dying
/// are**: a mis-lowered `for x in s` over a `Set` kills the test process, and a
/// `MinHeap` over `[3, 1, 2]` sums to `4349199564`, which is the worse one
/// because nothing reports it.
#[test]
fn a_for_reaches_every_member_of_every_iterable() {
    // The three simplest iterables first, so a regression here fails loudly.
    for (src, want, what) in [
        (
            "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  \
             var t = 0\n  for x in v { t = t * 10 + x }\n  t\n}\n",
            12,
            "Vec, in push order",
        ),
        (
            "fn main() -> Int {\n  var d = Deque()\n  d.push_back(1)\n  d.push_front(2)\n  \
             var t = 0\n  for x in d { t = t * 10 + x }\n  t\n}\n",
            21,
            "Deque, front to back",
        ),
        (
            "fn main() -> Int {\n  var t = 0\n  for i in 1..4 { t = t * 10 + i }\n  t\n}\n",
            123,
            "Range, ascending and half-open",
        ),
        // A `Set` is the one a wrong read kills the process on. Two members, so
        // a one-member answer is a different number from a two-member one.
        (
            "fn main() -> Int {\n  var s = Set()\n  s.insert(3)\n  s.insert(1)\n  \
             var t = 0\n  for x in s { t = t * 10 + x }\n  t\n}\n",
            13,
            "Set, ascending by member",
        ),
        // A `BitSet`'s members are bit positions, not objects: each is boxed by
        // the snapshot rather than copied out of the payload.
        (
            "fn main() -> Int {\n  var b = BitSet()\n  b.insert(5)\n  b.insert(2)\n  \
             var t = 0\n  for i in b { t = t * 10 + i }\n  t\n}\n",
            25,
            "BitSet, ascending",
        ),
        // The silently-wrong one. `[3, 1, 2]` in *pop* order is 1, 2, 3 — the
        // backing array is heap-ordered only at its root, so an indexed read of
        // it would answer in insertion-history order even if the read were
        // type-correct.
        (
            "fn main() -> Int {\n  var h = MinHeap()\n  h.push(3)\n  h.push(1)\n  h.push(2)\n  \
             var t = 0\n  for x in h { t = t * 10 + x }\n  t\n}\n",
            123,
            "MinHeap, ascending (pop order)",
        ),
        (
            "fn main() -> Int {\n  var h = MaxHeap()\n  h.push(1)\n  h.push(3)\n  h.push(2)\n  \
             var t = 0\n  for x in h { t = t * 10 + x }\n  t\n}\n",
            321,
            "MaxHeap, descending (pop order)",
        ),
        // A keyed collection yields the `(K, V)` pair `iter_item` has always
        // said it does; both halves are read here so a pair built in the wrong
        // order fails.
        (
            "fn main() -> Int {\n  var m = Map()\n  m.insert(1, 7)\n  m.insert(2, 8)\n  \
             var t = 0\n  for kv in m { t = t * 100 + kv.0 * 10 + kv.1 }\n  t\n}\n",
            1728,
            "Map, key then value",
        ),
        (
            "fn main() -> Int {\n  var c = Counter()\n  c.inc(1)\n  c.inc(1)\n  c.inc(2)\n  \
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
        "fn main() -> Int {\n  var g = read grid(char)\n  var n = 0\n  \
         for c in g { n = n + 1 }\n  n\n}\n",
        "ab\ncd\n",
    );
    assert!(!rt.has_pending_fault(), "Grid faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 4, "Grid, every cell row-major");

    // An empty one of each iterates zero times rather than once or forever —
    // the shape a length read off the wrong payload gets wrong first.
    for (src, what) in [
        ("var s = Set()\n  for x in s { n = n + 1 }", "Set"),
        ("var b = BitSet()\n  for x in b { n = n + 1 }", "BitSet"),
        ("var h = MinHeap()\n  for x in h { n = n + 1 }", "MinHeap"),
        ("var h = MaxHeap()\n  for x in h { n = n + 1 }", "MaxHeap"),
        ("var m = Map()\n  for kv in m { n = n + 1 }", "Map"),
        ("var c = Counter()\n  for kv in c { n = n + 1 }", "Counter"),
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
        "fn main() -> Int {\n  var s = Set()\n  s.insert(1)\n  s.insert(2)\n  \
         var n = 0\n  for x in s { n = n + 1\n s.insert(x + 10) }\n  n * 10 + s.len()\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 24, "two steps, four members afterwards");

    // The heap still holds everything it held: iterating is not popping.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var h = MinHeap()\n  h.push(3)\n  h.push(1)\n  \
         var n = 0\n  for x in h { n = n + 1 }\n  n * 10 + h.len()\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 22, "two steps, and the heap is untouched");

    // The snapshot has to survive the body's allocations: it is a `Gc` local the
    // loop keeps live, and if liveness missed it a collection would reclaim the
    // `Vec` being indexed. 300 members × an allocating body is well past the
    // initial 64 KiB threshold.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var s = Set()\n  var i = 0\n  \
         while i < 300 { s.insert(i)\n i = i + 1 }\n  \
         var t = 0\n  for x in s { t = t + x * 2 }\n  t\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 299 * 300, "sum(0..300) * 2");

    // …and so must the *pair* of snapshots a keyed collection walks, where the
    // keys must survive the values' allocation as well as the body's.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var m = Map()\n  var i = 0\n  \
         while i < 300 { m.insert(i, i)\n i = i + 1 }\n  \
         var t = 0\n  for kv in m { t = t + kv.0 + kv.1 }\n  t\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 299 * 300, "sum(0..300) twice over");
}

/// A `for`'s order is the order the collection's own accessors already promise,
/// which is what makes an iterating program's answer reproducible.
///
/// A `HashSet`'s iteration order is randomized **per process**: without a fixed
/// order the same program would sum the same numbers but concatenate them
/// differently on two runs. The three orders are each pinned against the
/// accessor that shares them.
#[test]
fn an_iterables_order_is_the_one_its_own_accessors_promise() {
    // A `Map`'s `for` visits exactly what `keys()`/`values()` list, in step.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var m = Map()\n  m.insert(3, 30)\n  m.insert(1, 10)\n  \
         m.insert(2, 20)\n  var ks = m.keys()\n  var vs = m.values()\n  \
         var i = 0\n  var agree = 1\n  \
         for kv in m { if kv.0 != ks.get(i) { agree = 0 }\n \
         if kv.1 != vs.get(i) { agree = 0 }\n i = i + 1 }\n  agree * 10 + i\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 13, "three steps, every one index-aligned");

    // A `Counter`'s is the same rule through the same helper.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var c = Counter()\n  c.inc(2)\n  c.inc(1)\n  c.inc(1)\n  \
         var ks = c.keys()\n  var i = 0\n  var agree = 1\n  \
         for kv in c { if kv.0 != ks.get(i) { agree = 0 }\n i = i + 1 }\n  agree * 10 + i\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 12);

    // A heap's is pop order, which is the one order here that is *meaningful*
    // rather than merely fixed — so it is asserted as a sequence and not only as
    // a set. Popping the same heap answers the same sequence.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var h = MinHeap()\n  h.push(5)\n  h.push(1)\n  h.push(9)\n  \
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
        "fn main() -> Int {\n  var s = Set()\n  s.insert(1)\n  s.insert(2)\n  s.insert(3)\n  \
         var t = 0\n  for x in s { t = t * 10 + x }\n  t\n}\n",
    );
    let (rt2, backward) = run_main(
        "fn main() -> Int {\n  var s = Set()\n  s.insert(3)\n  s.insert(2)\n  s.insert(1)\n  \
         var t = 0\n  for x in s { t = t * 10 + x }\n  t\n}\n",
    );
    assert!(!rt.has_pending_fault() && !rt2.has_pending_fault());
    assert_eq!(forward.as_int(), backward.as_int());
    assert_eq!(forward.as_int(), 123);
}

/// A hashed collection walks its `Int` keys in numeric order, not in the order
/// they print.
///
/// Ordering by the *rendered* member walks a `Set` holding 9, 10, 100 and 2 as
/// `10, 100, 2, 9` — nothing faults and nothing prints suspiciously, and a solve
/// that folds its members simply computes a different number. The single-digit
/// cases above cannot see it, because for one digit the two orders agree; these
/// keys are chosen so they do not.
#[test]
fn a_hashed_collection_orders_its_int_keys_numerically() {
    for inserts in [
        "s.insert(9)\n  s.insert(10)\n  s.insert(100)\n  s.insert(2)",
        "s.insert(2)\n  s.insert(100)\n  s.insert(10)\n  s.insert(9)",
    ] {
        let (rt, result) = run_main(&format!(
            "fn main() -> Int {{\n  var s = Set()\n  {inserts}\n  \
             var t = 0\n  for x in s {{ t = t * 1000 + x }}\n  t\n}}\n"
        ));
        assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
        assert_eq!(
            result.as_int(),
            2_009_010_100,
            "2, 9, 10, 100 — not the lexicographic 10, 100, 2, 9"
        );
    }

    // A `Map`'s keys are the same rule through the same helper, and a `Counter`
    // is the third caller of it.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var m = Map()\n  m.insert(9, 1)\n  m.insert(10, 2)\n  \
         m.insert(2, 3)\n  var ks = m.keys()\n  \
         ks[0] * 10000 + ks[1] * 100 + ks[2]\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 20910, "2, 9, 10");
}

/// A `Set` and the `Vec` its members sort into are **one** sequence.
///
/// `sorted()` goes through the element descriptor's `compare`, and the container
/// has to use the same order: a container ordered by the rendered form disagrees
/// with `sorted()` for any `Set[Int]` whose keys have different digit counts. An
/// order that fixed only the *printing* would still fail this, because both
/// sides here are folded by a `for`.
#[test]
fn a_set_and_its_sorted_vec_agree() {
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var s = Set()\n  s.insert(9)\n  s.insert(10)\n  \
         s.insert(100)\n  s.insert(2)\n  \
         var a = 0\n  for x in s { a = a * 1000 + x }\n  \
         var b = 0\n  for x in s.to_vec().sorted() { b = b * 1000 + x }\n  \
         if a == b { a } else { 0 }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(
        result.as_int(),
        2_009_010_100,
        "walking the set and walking its sorted members answer the same sequence"
    );
}

/// ADR-062's asymmetry: **one `for` body serves every iterable it is given**,
/// because the iterator stays quantified and monomorphization makes one clone
/// per iterable kind — and each clone picks its own [`IterPlan`] from a concrete
/// ctor.
///
/// `a_for_over_an_unannotated_parameter_runs_against_each_iterable_it_is_given`
/// covers `Vec`, `Deque` and `Range`; this covers the rest.
#[test]
fn a_for_over_an_unannotated_parameter_reaches_each_iterable_it_is_given() {
    // One source function, four ctors, all four in one program — so the clones
    // have to be distinct and each has to select its own accessor pair.
    let (rt, result) = run_main(
        "fn total(c) { var t = 0\n for x in c { t = t + x }\n t }\n\
         fn main() -> Int {\n  \
         var v = Vec()\n  v.push(1)\n  \
         var s = Set()\n  s.insert(2)\n  \
         var b = BitSet()\n  b.insert(4)\n  \
         var h = MinHeap()\n  h.push(8)\n  \
         total(v) + total(s) + total(b) + total(h)\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 15, "each clone reached its own members");

    // …including the paired plan, whose item is a tuple rather than an element.
    let (rt, result) = run_main(
        "fn tally(c) { var t = 0\n for kv in c { t = t + kv.1 }\n t }\n\
         fn main() -> Int {\n  \
         var m = Map()\n  m.insert(1, 5)\n  \
         var c = Counter()\n  c.inc(9)\n  \
         tally(m) * 10 + tally(c)\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 51);
}

/// A fused `enumerate()`/`zip()` pair holds both of its halves.
///
/// MIR emits `AllocKind::Tuple { ty: MirType::Opaque, … }` for the fused
/// pipelines, and `praxis_alloc_tuple` sizes the payload from the schema — so a
/// codegen that degraded `Opaque` to a *zero-element* schema would have both
/// `praxis_tuple_set` calls write into a zero-length `items`, and
/// `[10, 20].enumerate()` would answer `[(), ()]` out of a documented §6.3
/// combinator with nothing reported.
///
/// What this pins is that the *values* survive — the schema keeps its arity and
/// says "no static type" per slot, which ADR-066 made a thing a slot can say.
#[test]
fn a_fused_pair_carries_both_of_its_halves() {
    // `enumerate` pairs an index with an element, so reading `.0` and `.1` with
    // different weights fails on a swap as well as on a drop.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var v = Vec()\n  v.push(10)\n  v.push(20)\n  v.push(30)\n  \
         var t = 0\n  for p in v.enumerate() { t = t + p.0 * 100 + p.1 }\n  t\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 360, "(0,10) + (1,20) + (2,30), weighted");

    // `zip` is the other producer of an `Opaque`-typed pair.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var a = Vec()\n  a.push(1)\n  a.push(2)\n  \
         var b = Vec()\n  b.push(30)\n  b.push(40)\n  \
         var t = 0\n  for p in a.zip(b) { t = t + p.0 * 100 + p.1 }\n  t\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 370, "(1,30) + (2,40), weighted");

    // The pairs are also *values*: two runs of the same pipeline are equal, and
    // one whose halves differ is not — so the elements reach equality's
    // element-wise walk and are not compared as two empty tuples.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  \
         var w = Vec()\n  w.push(1)\n  w.push(9)\n  \
         var same = v.enumerate() == v.enumerate()\n  \
         var diff = v.enumerate() == w.enumerate()\n  \
         if same { 10 } else { 0 } + if diff { 1 } else { 0 }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 10, "equal to itself, unequal to the other");
}

/// A record pattern reads *fields* and a tuple pattern reads *elements* — the
/// half no type test can see, because the two are different runtime symbols
/// (`praxis_record_field` and `praxis_tuple_get`) and a pattern that picked the
/// wrong one would type-check identically.
///
/// Every component is weighted differently, so reading the right slots in the
/// wrong order fails as loudly as dropping one.
#[test]
fn a_record_and_a_tuple_pattern_read_the_components_they_name() {
    // A record, punned. `x * 100 + y` is a different answer from `y * 100 + x`.
    let (rt, result) = run_main(
        "struct P { x: Int, y: Int }\n\
         fn main() -> Int {\n  var p = P { x: 3, y: 4 }\n  \
         match p { P { x, y } => x * 100 + y }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 304);

    // …and explicit, with the fields written in the *other* order, so a
    // lowering that paired sub-patterns by position rather than by declared
    // field index answers 403.
    let (rt, result) = run_main(
        "struct P { x: Int, y: Int }\n\
         fn main() -> Int {\n  var p = P { x: 3, y: 4 }\n  \
         match p { P { y: b, x: a } => a * 100 + b }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 304, "bound by field name, not by position");

    // A tuple, by position.
    let (rt, result) =
        run_main("fn main() -> Int {\n  var t = (3, 4)\n  match t { (a, b) => a * 100 + b }\n}\n");
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 304);

    // A literal sub-pattern *selects* an arm: the components are tested, not
    // merely bound, so the first arm has to fail on its first element.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var t = 0\n  \
         for n in 1..4 {\n    var p = (n, n * 10)\n    \
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
         fn main() -> Int {\n  var o = Some((P { x: 3, y: 4 }, 5))\n  \
         match o { Some((P { x, y }, k)) => x * 100 + y * 10 + k, None => 0 }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 345);

    // A field the pattern does not name is a wildcard, and the ones it does are
    // still read from their own slots — a padded row must not shift the rest.
    let (rt, result) = run_main(
        "struct P { a: Int, b: Int, c: Int }\n\
         fn main() -> Int {\n  var p = P { a: 1, b: 2, c: 3 }\n  \
         match p { P { c } => c }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3, "the third field, not the first");

    // A record pattern binds a field of any type, not only the scalar the
    // arithmetic above could hide a mis-read of.
    let (rt, result) = run_main(
        "struct Tagged { name: Text, n: Int }\n\
         fn main() -> Int {\n  var t = Tagged { name: \"abc\", n: 7 }\n  \
         match t { Tagged { name, n } => name.len() * 10 + n }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 37);
}

/// `min=` keeps the smaller value, `max=` the larger, and an absent entry
/// accepts the first — the half no type test can see.
#[test]
fn an_updating_store_keeps_the_better_value_and_accepts_the_first() {
    // `min=` on a key that already has a value: the smaller wins, whichever
    // order the candidates arrive in, and a *worse* candidate changes nothing.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var d = Map()\n  d[\"a\"] = 10\n  \
         d[\"a\"] min= 4\n  d[\"a\"] min= 7\n  d[\"a\"] min= 9\n  d[\"a\"]\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 4);

    // `max=` is its dual, and the two are different wrappers: a program that
    // computed one with the other answers 4 here.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var b = Map()\n  b[\"a\"] = 10\n  \
         b[\"a\"] max= 4\n  b[\"a\"] max= 17\n  b[\"a\"] max= 9\n  b[\"a\"]\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 17);

    // **An absent entry accepts the first value** (§6.2), and it must not fault:
    // a subscript *read* of an absent key does (§4.7), which is exactly why this
    // cannot be a read-modify-write and is a row of its own.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var d = Map()\n  d[\"fresh\"] min= 42\n  \
         var b = Map()\n  b[\"fresh\"] max= 7\n  d[\"fresh\"] * 100 + b[\"fresh\"]\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 4207);

    // …and the first value is accepted *whatever* it is: a later, larger
    // candidate does not replace it under `min=`.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var d = Map()\n  d[\"k\"] min= 3\n  d[\"k\"] min= 100\n  d[\"k\"]\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);

    // Several keys, and a key computed by an expression — the place is a real
    // subscript and not a name in disguise.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var d = Map()\n  var i = 0\n  \
         while i < 6 {\n    d[i % 3] min= 10 - i\n    i = i + 1\n  }\n  \
         d[0] * 100 + d[1] * 10 + d[2]\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 765, "keys 0,1,2 keep 7,6,5");

    // §6.2's own shape: a Dijkstra-style relaxation through a generic helper, so
    // the deferred receiver reaches the backend as well as the type checker.
    let (rt, result) = run_main(
        "fn relax(dist, key, candidate) {\n  dist[key] min= candidate\n}\n\
         fn main() -> Int {\n  var distance = Map()\n  \
         relax(distance, 1, 7)\n  relax(distance, 1, 3)\n  relax(distance, 2, 9)\n  \
         distance[1] * 100 + distance[2]\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 309);

    // The update does not read: a `min=` on a map with an absent key runs where
    // `d[k] = d[k] + 1` would fault, and the map still holds one entry per key.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var d = Map()\n  d[\"a\"] min= 5\n  d[\"a\"] min= 5\n  d.len()\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

/// A destructuring `for` binding reads the components it names, once per step —
/// the half no type test can see.
#[test]
fn a_destructuring_for_binding_reads_each_item_apart() {
    // `for (k, v) in m` — §6.2's shape for walking a map, weighted so a swapped
    // pair or a dropped half is a different answer.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var m = Map()\n  m[1] = 20\n  m[3] = 40\n  \
         var t = 0\n  for (k, v) in m { t = t + k * 100 + v }\n  t\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 460, "(1,20) + (3,40), weighted");

    // …over a `Vec` of pairs, which is the in-place plan rather than the paired
    // snapshot — the two lowerings meet the same binding.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var v = Vec()\n  v.push((1, 20))\n  v.push((3, 40))\n  \
         var t = 0\n  for (a, b) in v { t = t + a * 100 + b }\n  t\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 460);

    // A record pattern in the header, and a field the pattern does not name.
    let (rt, result) = run_main(
        "struct P { x: Int, y: Int, z: Int }\n\
         fn main() -> Int {\n  var ps = Vec()\n  ps.push(P { x: 1, y: 2, z: 3 })\n  \
         ps.push(P { x: 4, y: 5, z: 6 })\n  var t = 0\n  \
         for P { z, x } in ps { t = t + x * 10 + z }\n  t\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 13 + 46);

    // Nested, and mutated: the binding is a fresh read each step, so a name bound
    // in one step does not leak into the next.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var v = Vec()\n  v.push((1, (2, 3)))\n  v.push((4, (5, 6)))\n  \
         var t = 0\n  for (a, (b, c)) in v { t = t * 10 + a + b + c }\n  t\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 6 * 10 + 15);

    // A bare name still binds the whole item, and the pair is still readable
    // with `.0`/`.1` — the spelling every existing program uses.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var m = Map()\n  m[1] = 20\n  \
         var t = 0\n  for kv in m { t = t + kv.0 * 100 + kv.1 }\n  t\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 120);

    // The destructured names survive an allocation inside the body: they are
    // real slots, not borrowed views into the item.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var m = Map()\n  m[1] = 2\n  m[3] = 4\n  \
         var t = 0\n  for (k, v) in m {\n    var scratch = Vec()\n    var i = 0\n    \
         while i < 50 { scratch.push(i)\n i = i + 1 }\n    t = t + k * 10 + v\n  }\n  t\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 12 + 34);
}

// ---------------------------------------------------------------------------
// The `Option` contract, end to end.
// ---------------------------------------------------------------------------

/// `Map.get` answers `Option[V]` (§5.7 writes that signature literally), so a
/// program tells absence from a value by *matching* rather than by comparing the
/// answer against something it is not.
///
/// The runtime builds the `Option` through its own `option_schema`, whose `Some`
/// slot is unknown, while the program's arms are compiled against the codegen's
/// `Option[Int]` schema. That the two meet at all is `EnumSchema::same_type`'s
/// null-slot rule; this is where it earns its keep.
#[test]
fn an_absent_map_get_matches_none_and_a_present_one_matches_some() {
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var m = Map()\n  m.insert(1, 10)\n  \
         match m.get(1) {\n    Some(v) => v,\n    None => 0 - 1,\n  }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 10);

    let (rt, result) = run_main(
        "fn main() -> Int {\n  var m = Map()\n  m.insert(1, 10)\n  \
         match m.get(2) {\n    Some(v) => v,\n    None => 0 - 1,\n  }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), -1);

    // The value inside the `Some` is the real one, whatever its type: a `Text`
    // value comes back as a `Text`, not as an `i64` read of its buffer pointer.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var m = Map()\n  m.insert(1, \"abc\")\n  \
         match m.get(1) {\n    Some(v) => v.len(),\n    None => 0 - 1,\n  }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);

    // …and `map[key]` is still the other half of §4.7's sentence: the
    // assertion-like spelling, which faults rather than answering `None`.
    let (rt, _) = run_main("fn main() -> Int {\n  var m = Map()\n  m[7]\n}\n");
    assert!(
        rt.has_pending_fault(),
        "§4.7: indexing a missing key faults"
    );
}

/// The same contract under a *tuple* payload: `Grid.find` answers
/// `Option[(Int, Int)]`, and the point survives being carried inside the enum.
#[test]
fn an_absent_grid_find_is_none_and_a_hit_is_some_of_the_point() {
    let src = "fn main() -> Int {\n  var g = read matrix(int)\n  \
               match g.find(4) {\n    Some((x, y)) => x * 10 + y,\n    None => 0 - 1,\n  }\n}\n";
    let (rt, result) = run_main_with_input(src, "1 2\n3 4\n");
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 11, "4 is at (1, 1)");

    let src = "fn main() -> Int {\n  var g = read matrix(int)\n  \
               match g.find(99) {\n    Some((x, y)) => x * 10 + y,\n    None => 0 - 1,\n  }\n}\n";
    let (rt, result) = run_main_with_input(src, "1 2\n3 4\n");
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), -1);
}

/// Enum identity at the one place a program can observe it: **equality against a
/// value the two producers built independently**.
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
        "fn main() -> Int {\n  var m = Map()\n  m.insert(1, 10)\n  \
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
        "fn main() -> Int {\n  var m = Map()\n  m.insert(1, 10)\n  \
         if m.get(1) == Some(11) { 1 } else { 0 }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);

    // Absence is `None`, and `None` is not `Some` of anything.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var m = Map()\n  m.insert(1, 10)\n  \
         if m.get(2) == None { 1 } else { 0 }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);

    // A `Text` payload stays a `Text`: the schema slot the runtime filled is
    // unknown, so the value's own descriptor decides, and it is never wrong.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var m = Map()\n  m.insert(1, \"x\")\n  \
         if m.get(1) == Some(\"x\") { 1 } else { 0 }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1);
}

/// A field read on an unannotated parameter reads the field, and the call site
/// is what says which record it is.
///
/// The half no type test can see: that the *index* is right, at each field,
/// through each shape a record can be reached in.
#[test]
fn a_field_read_on_an_unannotated_parameter_reads_that_records_field() {
    // §4.9's example, verbatim in shape: both fields, weighted so a swapped index
    // is a different answer.
    let (rt, result) = run_main(
        "struct P { x: Int, y: Int }\n\
         fn dist(a) -> Int { a.x * 10 + a.y }\n\
         fn main() -> Int { dist(P { x: 3, y: 7 }) }\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 37);

    // A field that is not the first, on a record with a `Text` between two `Int`s
    // — the index is read from the definition and not from the order of use.
    let (rt, result) = run_main(
        "struct R { a: Int, tag: Text, b: Int }\n\
         fn back(r) -> Int { r.b }\n\
         fn main() -> Int { back(R { a: 1, tag: \"t\", b: 9 }) }\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 9);

    // Chained through a second unannotated parameter: `outer.inner` is a deferred
    // read whose result is itself a record, and `.x` on it is a second deferred
    // read that only the first one's discharge can resolve.
    let (rt, result) = run_main(
        "struct Inner { x: Int }\n\
         struct Outer { inner: Inner }\n\
         fn deep(o) -> Int { o.inner.x }\n\
         fn main() -> Int { deep(Outer { inner: Inner { x: 42 } }) }\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);

    // The field's own type is what the read produces, so a `Text` field reached
    // through an unannotated parameter is usable as a `Text`.
    let (rt, result) = run_main(
        "struct N { name: Text, n: Int }\n\
         fn label(v) -> Int { v.name.len() }\n\
         fn main() -> Int { label(N { name: \"abcd\", n: 0 }) }\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 4);

    // The record may arrive from a collection rather than a literal — the call
    // site pins it either way.
    let (rt, result) = run_main(
        "struct P { x: Int, y: Int }\n\
         fn sumx(p) -> Int { p.x }\n\
         fn main() -> Int {\n  var ps = Vec()\n  ps.push(P { x: 5, y: 0 })\n  \
         ps.push(P { x: 6, y: 0 })\n  var t = 0\n  \
         for p in ps { t = t + sumx(p) }\n  t\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 11);

    // …and the read survives 200 allocations between the call and the use, so the
    // receiver is a rooted value and not a stale view.
    let (rt, result) = run_main(
        "struct P { x: Int, y: Int }\n\
         fn far(p) -> Int {\n  var scratch = Vec()\n  var i = 0\n  \
         while i < 200 { scratch.push(P { x: i, y: i })\n i = i + 1 }\n  p.x + p.y\n}\n\
         fn main() -> Int { far(P { x: 11, y: 22 }) }\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 33);
}

/// A destructuring closure parameter reads the components it names, once per
/// call — the half no type test can see.
///
/// The parameter still arrives as one value; the pattern takes it apart inside the
/// closure, through a one-arm `match` on the parameter's own slot. Everything here
/// is weighted so a swapped component or a dropped one is a different answer.
#[test]
fn a_destructuring_closure_parameter_reads_each_argument_apart() {
    // Appendix D's shape: a pair destructured in a `map`.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var v = Vec()\n  v.push((1, 20))\n  v.push((3, 40))\n  \
         v.map(|(a, b)| a * 100 + b).sum()\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 120 + 340);

    // A record pattern, with a field the pattern does not name — the padded row
    // must not shift the ones it does.
    let (rt, result) = run_main(
        "struct P { x: Int, y: Int, z: Int }\n\
         fn main() -> Int {\n  var f = |P { z, x }| x * 10 + z\n  \
         f(P { x: 1, y: 2, z: 3 })\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 13);

    // Several parameters, only some of them patterns, in both orders — each
    // `match` wraps its own argument and none of them shifts another.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var f = |(a, b), c| a * 100 + b * 10 + c\n  f((1, 2), 3)\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 123);
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var f = |a, (b, c)| a * 100 + b * 10 + c\n  f(1, (2, 3))\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 123);

    // Nested, and through a record — the reads chain.
    let (rt, result) = run_main(
        "struct P { at: (Int, Int), w: Int }\n\
         fn main() -> Int {\n  var f = |P { at: (x, y), w }| x * 100 + y * 10 + w\n  \
         f(P { at: (1, 2), w: 3 })\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 123);

    // A wildcard component reads nothing, and the named one is still the one it
    // names.
    let (rt, result) = run_main("fn main() -> Int {\n  var f = |(_, b)| b\n  f((9, 4))\n}\n");
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 4);

    // The destructured names are real slots: they survive 200 allocations inside
    // the body, and they capture into a nested closure.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var f = |(a, b)| {\n    var scratch = Vec()\n    var i = 0\n    \
         while i < 200 { scratch.push((i, i))\n i = i + 1 }\n    var g = |n| n + a * 10 + b\n    \
         g(0)\n  }\n  f((1, 2))\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 12);

    // A bare-name parameter is untouched — same slot, same reads, same answer.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var v = Vec()\n  v.push((1, 20))\n  \
         v.map(|kv| kv.0 * 100 + kv.1).sum()\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 120);
}

/// A zero-parameter closure runs, captures, and `||` still short-circuits.
///
/// A zero-parameter closure is spelled with a token that already had a meaning,
/// so the half that matters is the half that must not change: `a || b` evaluates
/// `b` only when `a` is false, and a program that observes the difference is the
/// only thing that can say so.
#[test]
fn a_zero_parameter_closure_runs_and_the_or_it_is_spelled_like_still_short_circuits() {
    // The closure itself: called, and called twice.
    let (rt, result) = run_main("fn main() -> Int {\n  var f = || 5\n  f() + f()\n}\n");
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 10);

    // §4.2's shadowing example: the closure keeps the binding it captured, and a
    // zero-parameter closure is the shape §4.2 writes it in.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var a = 4\n  var show_old = || a\n  var a = 9\n  \
         show_old() * 10 + a\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 49);

    // **`||` still short-circuits**, measured by a side effect the skipped side
    // would leave: `true || …` must not run the right operand, and `false || …`
    // must. A `var` captured by cell is what makes the count visible.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var n = 0\n  var bump = || { n = n + 1\n true }\n  \
         var a = true || bump()\n  var b = false || bump()\n  \
         if a { 0 } else { 0 }\n  if b { n } else { 100 }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(
        result.as_int(),
        1,
        "only the `false ||` ran its right operand"
    );

    // …and `&&` is unaffected, from the other side of the precedence table.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var n = 0\n  var bump = || { n = n + 1\n true }\n  \
         var a = false && bump()\n  if a { 100 } else { n }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);

    // A zero-parameter closure nests inside another one — the body of a `||` is
    // an ordinary expression, so `|| || 7` is a closure returning a closure.
    let (rt, result) = run_main("fn main() -> Int {\n  var f = || || 7\n  f()()\n}\n");
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 7);

    // …and it survives allocation pressure between its creation and its call.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var n = 6\n  var f = || n * 7\n  var scratch = Vec()\n  \
         var i = 0\n  while i < 200 { scratch.push(i)\n i = i + 1 }\n  f()\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

/// A `_` parameter keeps its slot, so the parameter after it gets the argument
/// the call actually passed.
///
/// `_` binds no name (ADR-049 D7), and `lower_param` must not read that as
/// "there is no parameter": dropping it from the slot list while the function's
/// *type* still has it makes the lowered body's arity and its signature's
/// disagree. The two halves fail differently — a closure gives a **silently
/// wrong answer** (`|_, b| b + 1` applied to `(9, 5)` answers `10`, because `b`
/// reads argument one), and a `fn` dies in the Cranelift verifier.
///
/// Every assertion below is a *value*, not a "does not crash": a shifted argument
/// list has to come out as the wrong number. The digit-place encodings
/// (`a * 100 + c`) are there so a swap, a drop and a duplication are three
/// different failures.
#[test]
fn a_wildcard_parameter_keeps_its_slot_so_later_parameters_do_not_shift() {
    // The closure case.
    let (rt, result) = run_main("fn main() -> Int {\n  var f = |_, b| b + 1\n  f(9, 5)\n}\n");
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 6, "`b` must be the *second* argument");

    // The same shape as a `fn`.
    let (rt, result) = run_main("fn g(_, b) -> Int { b + 1 }\nfn main() -> Int { g(9, 5) }\n");
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 6);

    // A wildcard in the middle: the parameters on *both* sides of it stay put.
    let (rt, result) =
        run_main("fn main() -> Int {\n  var f = |a, _, c| a * 100 + c\n  f(1, 2, 3)\n}\n");
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 103);
    let (rt, result) =
        run_main("fn h(a, _, c) -> Int { a * 100 + c }\nfn main() -> Int { h(1, 2, 3) }\n");
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 103);

    // Two wildcards are two slots, not one: each `_` is minted at its own range,
    // so they do not collide and the shift is two wide rather than one.
    let (rt, result) = run_main("fn main() -> Int {\n  var f = |_, _, c| c * 7\n  f(1, 2, 3)\n}\n");
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 21);
    let (rt, result) =
        run_main("fn k(_, _, c) -> Int { c * 7 }\nfn main() -> Int { k(1, 2, 3) }\n");
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 21);

    // A trailing wildcard has nothing after it to shift, and must still be
    // accepted rather than becoming an arity mismatch.
    let (rt, result) = run_main("fn main() -> Int {\n  var f = |a, _| a * 2\n  f(21, 99)\n}\n");
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);

    // ADR-049 D7's own spelling, `|_| 0`, through the pipeline the doc writes it
    // for.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  v.push(2)\n  \
         v.map(|_| 7).sum()\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 14);

    // A wildcard *component* is a different thing and keeps working: the pattern
    // owns the argument, and the `_` inside it has no slot of its own.
    let (rt, result) =
        run_main("fn main() -> Int {\n  var f = |(_, b), c| b * 10 + c\n  f((9, 4), 3)\n}\n");
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 43);

    // A wildcard next to a destructuring parameter: two different anonymous
    // slots, both at their own ranges, neither shifting the other.
    let (rt, result) =
        run_main("fn main() -> Int {\n  var f = |_, (a, b)| a * 10 + b\n  f(9, (1, 2))\n}\n");
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 12);

    // The wildcard's argument is still *evaluated* — D7's rule that a binder a
    // program does not name still runs what it is given.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var v = Vec()\n  var bump = |n| { v.push(n)\n n }\n  \
         var f = |_, b| b\n  var r = f(bump(1), 5)\n  r * 10 + v.len()\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 51);
}

// --- §7.4's atomic parsers, end to end --------------------------------------

/// Every one of §7.4's ten atomic parsers runs, `uint`, `float`, `byte` and
/// `identifier` included.
///
/// This is the half neither the type test nor the runtime unit test can see: a
/// compiled program reading real input through the real ABI, so the value's
/// descriptor has to be right as well as its type.
#[test]
fn every_atomic_the_design_requires_runs_in_a_compiled_program() {
    // `uint` is an Int, and arithmetic on it works.
    let (rt, result) = run_main_with_input(
        "fn main() -> Int {\n  var n = read uint\n  n + 1\n}\n",
        "41",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);

    // A leading `-` is not a `uint`: §7.4's non-negativity is the parse rule,
    // because `ScalarType::UInt` has no runtime object to be typed with.
    let (rt, _) = run_main_with_input(
        "fn main() -> Int {\n  var n = read uint\n  n + 1\n}\n",
        "-1",
    );
    assert!(rt.has_pending_fault(), "`uint` must refuse a negative");

    // `float` is a Float, and it reads a fraction.
    let (rt, result) = run_main_with_input(
        "fn main() -> Float {\n  var x = read float\n  x + 0.5\n}\n",
        "3.25",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_float(), 3.75);

    // `byte` is a Byte in 0..=255.
    let (rt, result) = run_main_with_input(
        "fn main() -> Int {\n  var bs = read csv(byte)\n  bs.len()\n}\n",
        "0,127,255",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);
    let (rt, _) = run_main_with_input("fn main() -> Int {\n  var b = read byte\n  1\n}\n", "256");
    assert!(rt.has_pending_fault(), "256 is not a byte");

    // `identifier` is a Text under §4.1's one character class, so a Unicode
    // name is a name — a deliberate widening of §7.4's "ASCII-like by default".
    let (rt, result) = run_main_with_input(
        "fn main() -> Int {\n  var names = read lines(identifier)\n  names.len()\n}\n",
        "alpha\nλx\n_beta9\n",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 3);

    // And in a template capture, which is the shape they will actually be
    // written in.
    let (rt, result) = run_main_with_input(
        "fn main() -> Int {\n  var rows = read lines(`{name:identifier}={n:uint}`)\n  \
         var t = 0\n  for r in rows { t = t + r.n }\n  t\n}\n",
        "a=1\nb=2\nc=39\n",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 42);
}

/// **ADR-085.** `Text + Text` is concatenation, end to end through the JIT.
///
/// Every assertion is on the *bytes*, not the length: `c.len()` alone passes for
/// a wrapper that concatenates in the wrong order, and structural equality with
/// a literal written the right way round does not. So each case compares against
/// the answer and returns a length only to carry a distinguishable number back.
///
/// `praxis_text_concat` is the one new wrapper. It is declared `Allocates`, so
/// the operands must be rooted across the call — the nested case
/// (`(a + b) + (c + d)`) is what would collect a half-built temporary if they
/// were not.
#[test]
fn text_concatenation_joins_two_texts() {
    // The basic case, checked as bytes.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var c = \"ab\" + \"cde\"\n  \
         if c == \"abcde\" { c.len() } else { 0 - 1 }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 5, "\"ab\" + \"cde\" is \"abcde\"");

    // Order matters, and a length cannot see it.
    let (rt, result) =
        run_main("fn main() -> Int {\n  if \"ab\" + \"cde\" == \"cdeab\" { 1 } else { 0 }\n}\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0, "concatenation is not commutative");

    // Left-associative chaining, and four live temporaries across an allocating
    // call.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var a = \"1\"\n  var b = \"2\"\n  var c = \"3\"\n  var d = \"4\"\n  \
         if (a + b) + (c + d) == \"1234\" { 1 } else { 0 }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1, "(a + b) + (c + d) is \"1234\"");

    // The empty text is the identity, in both positions.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  if \"\" + \"x\" == \"x\" && \"x\" + \"\" == \"x\" { 1 } else { 0 }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1, "the empty Text is the identity");

    // Multi-byte characters: `len()` counts chars (§4.3), and joining two valid
    // UTF-8 payloads cannot split one — which is why this wrapper needs no
    // `InvalidText` fault. Since ADR-111 `praxis_alloc_text` needs none either,
    // for the neighbouring reason: its bytes are the caller's promise, and the
    // literals below are where that promise is kept.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var c = \"héllo\" + \" wörld\"\n  \
         if c == \"héllo wörld\" { c.len() } else { 0 - 1 }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 11, "eleven characters, thirteen bytes");

    // `+=` on a `Text` binding is the same operator (ADR-085), including through
    // the `VarCell` a captured `var` lives in (§4.2).
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var s = \"a\"\n  var read_it = || s\n  s += \"b\"\n  s += \"c\"\n  \
         if s == \"abc\" && read_it() == \"abc\" { s.len() } else { 0 - 1 }\n}\n",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(
        result.as_int(),
        3,
        "a captured var observes each concatenation"
    );

    // A concatenation is an ordinary `Text`, so it hashes and compares
    // structurally like any other (§5.5) — a `Map` keyed by one must find the
    // entry a literal key inserted.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var m = Map()\n  m.insert(\"ab\", 7)\n  m[\"a\" + \"b\"]\n}\n",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(
        result.as_int(),
        7,
        "a built Text keys the same entry a literal does"
    );
}

/// **ADR-111's behavioural backstop.** A multi-byte `Text` literal, in a loop,
/// still round-trips through the JIT.
///
/// Two things this catches that no MIR-shape test can. First, the cold refusal:
/// `praxis_alloc_text` *aborts* on bytes that are not UTF-8, so a botched edit
/// to its `from_utf8` — one that read the wrong length, say, or measured chars
/// where it wanted bytes — turns every literal in every program into a process
/// abort rather than a wrong answer. Multi-byte literals are where that
/// distinction lives; an ASCII-only test cannot tell a byte length from a char
/// count. Second, the ADR-108 hoist applies to `Text`, so the literals below
/// are allocated **once, in the preheader**, and the loop compares the same
/// object on every iteration. That sharing is sound because a `Text` payload is
/// immutable and `text_equals` is a structural byte comparison — this is the
/// test that says so end to end, where
/// `a_text_literal_in_a_loop_is_hoisted_now_that_its_alloc_cannot_fault`
/// (`praxis-mir`'s `build.rs`) only says the instruction moved.
///
/// The assertion is on the *count* rather than on a `Bool`: a hoist that shared
/// the wrong object, or a comparison that answered identity, would still make
/// one iteration's answer look right.
#[test]
fn a_multibyte_text_literal_still_round_trips_through_the_jit() {
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var hits = 0\n  var i = 0\n  while i < 5 {\n    \
         var s = \"héllo wörld\"\n    if s == \"héllo wörld\" && s.len() == 11 { hits = hits + 1 }\n    \
         i = i + 1\n  }\n  hits\n}\n",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(
        result.as_int(),
        5,
        "a hoisted multi-byte literal is the same eleven characters on every iteration"
    );

    // And a `Map` keyed by one still finds the entry: hashing is structural, so
    // one shared object and five separate ones key the same slot.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var m = Map()\n  m.insert(\"ключ\", 9)\n  \
         var total = 0\n  var i = 0\n  while i < 3 {\n    total = total + m[\"ключ\"]\n    \
         i = i + 1\n  }\n  total\n}\n",
    );
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 27, "a shared Text key hashes structurally");
}

/// `sorted` orders through the element descriptor, and the receiver keeps its
/// own order.
///
/// The `Vec[Text]` case is why this cannot be written with integers alone. A
/// `Text` is a pointer-and-length structure, so a sort that reads the first
/// eight payload bytes compares *addresses* — which passes on `Vec[Int]` and
/// answers allocation order on `Vec[Text]`. The three texts are pushed in an
/// order whose allocations ascend `"b"`, `"a"`, `"c"`, so a payload sort answers
/// `"b"` here and this fails.
#[test]
fn a_sorted_vec_is_ordered_by_the_descriptor_and_the_source_is_untouched() {
    // Integers: the easy half, and the one a wrong implementation also passes.
    let src = "fn main() -> Int {\n  var v = Vec[Int]()\n  v.push(5)\n  v.push(1)\n  \
               v.push(3)\n  var s = v.sorted()\n  s.get(0) * 100 + s.get(1) * 10 + s.get(2)\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 135, "1, 3, 5 in that order");

    // Text: ordered by `compare`, not by the payload's first eight bytes.
    let src = "fn main() -> Text {\n  var v = Vec[Text]()\n  v.push(\"b\")\n  \
               v.push(\"a\")\n  v.push(\"c\")\n  v.sorted().get(0)\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(
        result.as_text(),
        "a",
        "ordered by the descriptor's `compare`"
    );

    // An empty Vec sorts to an empty Vec rather than faulting on a `compare` it
    // would never have called.
    let src = "fn main() -> Int {\n  var v = Vec[Int]()\n  v.sorted().len()\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);

    // The receiver keeps its own order: `sorted` answers a new Vec. `51` is
    // `v.get(0)` still 5 and `s.get(0)` already 1.
    let src = "fn main() -> Int {\n  var v = Vec[Int]()\n  v.push(5)\n  v.push(1)\n  \
               var s = v.sorted()\n  v.get(0) * 10 + s.get(0)\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 51, "the source Vec is not mutated");
}

/// `frequencies` counts every element, an absent key reads zero, and `unique`
/// keeps first occurrences.
///
/// The zero-default is what proves the result is a real `Counter` and not a
/// `Map` wearing the type (§6.2: "absent values read as zero"). `310` is three
/// 3s, one 4, and an absent 7.
#[test]
fn frequencies_counts_every_element_and_an_absent_key_reads_zero() {
    let src = "fn main() -> Int {\n  var v = Vec[Int]()\n  v.push(3)\n  v.push(3)\n  \
               v.push(4)\n  v.push(3)\n  v.push(9)\n  var c = v.frequencies()\n  \
               c[3] * 100 + c[4] * 10 + c[7]\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(
        result.as_int(),
        310,
        "three 3s, one 4, and an absent 7 is zero"
    );

    // `unique` in first-occurrence order, not sorted: `231` is two elements,
    // `3` first and `1` second — the order they were pushed, not `1, 3`.
    let src = "fn main() -> Int {\n  var v = Vec[Int]()\n  v.push(3)\n  v.push(1)\n  \
               v.push(3)\n  v.push(1)\n  var u = v.unique()\n  \
               u.len() * 100 + u.get(0) * 10 + u.get(1)\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(
        result.as_int(),
        231,
        "duplicates dropped, first occurrences kept"
    );
}

// ===========================================================================
// List literals and `Text` iteration
// ===========================================================================

/// A list literal **is** a `Vec`: allocated and pushed into, so everything a
/// `Vec` can do it can do.
///
/// Each case answers something a wrong lowering could not produce. The order
/// assertions are the load-bearing ones: a literal that allocated before
/// evaluating any element, or that pushed in reverse, still passes a `len()`
/// check and fails these.
#[test]
fn a_list_literal_builds_a_vec_of_its_elements_in_order() {
    for (src, want, what) in [
        (
            "fn main() -> Int {\n  var v = [1, 2, 3]\n  v.len()\n}\n",
            3,
            "three elements",
        ),
        // Positional: `123` and not `321`, so a reversed push order fails.
        (
            "fn main() -> Int {\n  var v = [1, 2, 3]\n  v.get(0) * 100 + v.get(1) * 10 + v.get(2)\n}\n",
            123,
            "elements in source order",
        ),
        (
            "fn main() -> Int {\n  var v = []\n  v.len()\n}\n",
            0,
            "the empty list",
        ),
        (
            "fn main() -> Int {\n  var v = [7]\n  v[0]\n}\n",
            7,
            "one element, read by subscript",
        ),
        // An element is an arbitrary expression, evaluated where it is written.
        (
            "fn main() -> Int {\n  var n = 4\n  var v = [n + 1, n * 2]\n  v.get(0) * 10 + v.get(1)\n}\n",
            58,
            "computed elements",
        ),
        // Nested: the inner literals are elements of the outer one.
        (
            "fn main() -> Int {\n  var v = [[1, 2], [3]]\n  v.len() * 100 + v.get(0).len() * 10 + v.get(1).len()\n}\n",
            221,
            "a list of lists",
        ),
        // The `Vec` a literal builds takes every `Vec` method, including the
        // ones that read the element descriptor.
        (
            "fn main() -> Int {\n  var v = [3, 1, 2]\n  var s = v.sorted()\n  \
             s.get(0) * 100 + s.get(1) * 10 + s.get(2)\n}\n",
            123,
            "sorted, which dispatches on the element descriptor",
        ),
        (
            "fn main() -> Int {\n  [1, 2, 3].sum()\n}\n",
            6,
            "a pipeline sink straight off the literal",
        ),
        // …and it is a real, mutable `Vec`: pushing into one works.
        (
            "fn main() -> Int {\n  var v = [1]\n  v.push(2)\n  v.len() * 10 + v.get(1)\n}\n",
            22,
            "a literal is mutable afterwards",
        ),
    ] {
        let (rt, result) = run_main(src);
        assert!(!rt.has_pending_fault(), "{what} faulted: {:?}", rt.fault());
        assert_eq!(result.as_int(), want, "{what}");
    }
}

/// A `for` over a list literal reaches every element, in order.
///
/// The literal is not bound to a name here, which is the shape worth its own
/// case: the `Vec` the loop walks is a temporary, and it has to stay rooted
/// across a body that allocates.
#[test]
fn a_for_over_a_list_literal_reaches_every_element() {
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var t = 0\n  for x in [1, 2, 3] { t = t * 10 + x }\n  t\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 123, "every element, in order");

    // An empty literal iterates zero times rather than once or forever.
    let (rt, result) =
        run_main("fn main() -> Int {\n  var n = 0\n  for x in [] { n = n + 1 }\n  n\n}\n");
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0, "an empty list iterates zero times");

    // The temporary must survive an allocating body across a collection: 300
    // pushes into a `Vec` the loop does not hold is well past the initial 64 KiB
    // threshold, so an unrooted iteration source would be reclaimed under it.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var t = 0\n  var i = 0\n  \
         while i < 300 { for x in [1, 2, 3] { var junk = Vec()\n junk.push(x)\n t = t + x }\n i = i + 1 }\n  t\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(
        result.as_int(),
        300 * 6,
        "the walked temporary stays rooted"
    );

    // A literal of a *heap* type: each element allocates, and the `Vec` being
    // built has to be rooted across those allocations.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var v = [\"aa\", \"b\", \"ccc\"]\n  var t = 0\n  \
         for s in v { t = t * 10 + s.len() }\n  t\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 213, "allocating elements, in order");
}

/// **A `for` over a `Text` yields its characters** (§4.13, ADR-086).
///
/// The loop indexes the `Text` in place through the
/// `praxis_text_len`/`praxis_text_get` pair that `t.len()` and `t[i]` already
/// call, so the two spellings cannot disagree — which is what the second case
/// asserts by comparing the loop's answer to the subscript's.
#[test]
fn a_for_over_a_text_yields_its_characters() {
    // `w`=119, `x`=120, `y`=121, `z`=122 — the sum is what a loop that skipped
    // or repeated a character gets wrong.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var n = 0\n  for c in \"wxyz\" { n = n + c.to_int() }\n  n\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 119 + 120 + 121 + 122);

    // The loop and the subscript are one answer: walking `t` and indexing it
    // must produce the same characters in the same order.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var t = \"abc\"\n  var i = 0\n  var same = 1\n  \
         for c in t { if c == t[i] { same = same } else { same = 0 }\n i = i + 1 }\n  \
         same * 10 + i\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 13, "three steps, each matching `t[i]`");

    // By Unicode scalar and not by byte, which is the property `text_get`'s own
    // tests pin for the subscript: "héllo" is five characters, and é is 233.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var n = 0\n  var first = 0\n  \
         for c in \"héllo\" { n = n + 1\n if n == 2 { first = c.to_int() } else { first = first } }\n  \
         n * 1000 + first\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 5233, "five scalars, and the second is é");

    // An empty `Text` iterates zero times — the shape a length read off the
    // wrong payload gets wrong first.
    let (rt, result) =
        run_main("fn main() -> Int {\n  var n = 0\n  for c in \"\" { n = n + 1 }\n  n\n}\n");
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0, "an empty Text iterates zero times");

    // A `Text` reached through a parameter is one more iterable the single
    // quantified body serves (ADR-062): `each` is cloned per iterable kind, and
    // this clone's symbols are `praxis_text_*`.
    let (rt, result) = run_main(
        "fn count(r) { var n = 0\n for x in r { n = n + 1 }\n n }\n\
         fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  \
         count(\"abc\") * 10 + count(v)\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(
        result.as_int(),
        31,
        "one body, a Text clone and a Vec clone"
    );
}

/// **ADR-115 end to end, on the `Text` shape the decision exists for.**
///
/// The texts a program indexes are mostly not literals — they are the input
/// parser's captures, which are `SourceSlice` views of the stdin buffer. The
/// scalar count lives on the owned payload, so a view answers `len()` and `[i]`
/// through its owner, and this is the test that the two representations give
/// one answer. Nothing here can distinguish O(1) from O(n) by timing; what it
/// pins is that the fast path is taken where it is right and refused where it
/// is not.
#[test]
fn a_parsed_text_reads_the_same_as_the_literal_it_came_from() {
    // ASCII input: every capture is a view of a one-byte-per-scalar owner, so
    // every `len()` and `[i]` takes the byte-index path.
    let (rt, result) = run_main_with_input(
        "fn main() -> Int {\n  var ws = read lines(word)\n  var n = 0\n  \
         var i = 0\n  while i < ws.len() {\n    var w = ws[i]\n    \
         for c in w { n = n + c.to_int() }\n    n = n + w.len()\n    i = i + 1\n  }\n  n\n}\n",
        "ab\ncde\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    let expect: i64 = "abcde".chars().map(|c| c as i64).sum::<i64>() + 2 + 3;
    assert_eq!(result.as_int(), expect, "five characters and two lengths");

    // The same program over input with a multi-byte scalar in it. The captures
    // "wörld" and "abc" are views of an owner that is *not* one byte per
    // scalar — including "abc", whose own bytes are — so both must fall back to
    // decoding and both must still answer in scalars. A view that inherited the
    // byte-index path from its own bytes would answer 6 for "wörld".
    let (rt, result) = run_main_with_input(
        "fn main() -> Int {\n  var ws = read lines(word)\n  var n = 0\n  \
         var i = 0\n  while i < ws.len() {\n    n = n * 10 + ws[i].len()\n    i = i + 1\n  }\n  n\n}\n",
        "wörld\nabc\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(
        result.as_int(),
        53,
        "five scalars then three, not six then three"
    );

    // And the characters themselves, walked out of a view whose owner has a
    // multi-byte scalar *before* the view starts — the case where a byte index
    // is not merely wide but misaligned.
    let (rt, result) = run_main_with_input(
        "fn main() -> Int {\n  var ws = read lines(word)\n  var n = 0\n  \
         for c in ws[1] { n = n + c.to_int() }\n  n\n}\n",
        "ö\nxyz\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 120 + 121 + 122, "x, y, z");
}

// ===========================================================================
// §3.5 — an interned `Int` literal is two loads, not a call and an allocation
// (`praxis_runtime::small_int`).
// ===========================================================================

/// An in-range literal evaluated a hundred times allocates nothing, and an
/// out-of-range one still allocates every time.
///
/// The pair is the point. Counting live objects after the loop is the only
/// end-to-end way to see that `Inst::ConstGc` reached the backend: the answer
/// (`100`) is identical either way, and a timing difference alone could not
/// distinguish "the literal stopped allocating" from "the pacer got luckier".
#[test]
fn an_interned_literal_costs_no_allocation_and_a_large_one_still_does() {
    // The harness installs an input `Text` before every run, so "allocated
    // nothing" is measured against a program that runs no loop at all rather
    // than against zero.
    let (control, _) = run_main("fn main() -> Int {\n  0\n}\n");
    let floor = control.heap().stats().live_count;

    // `i = i + 1`: the literal `1` is interned, `i` runs only to 100 so every
    // value it takes is interned too, and the loop allocates nothing at all.
    let (rt, result) =
        run_main("fn main() -> Int {\n  var i = 0\n  while i < 100 { i = i + 1 }\n  i\n}\n");
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 100);
    assert_eq!(
        rt.heap().stats().live_count,
        floor,
        "a loop over interned values must not allocate"
    );

    // The same loop shape with an accumulator that leaves the range: the
    // literals are still free, but every partial sum is a real object. This is
    // the half a regression that deleted the out-of-range branch would break.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var i = 0\n  var s = 0\n  \
         while i < 100 { s = s + 1000\n i = i + 1 }\n  s\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 100_000);
    assert!(
        rt.heap().stats().live_count > floor,
        "an out-of-range value is still a real allocation"
    );
}

// ===========================================================================
// ADR-113 — a *runtime* `Int` in range is the same table read, inline.
//
// `Inst::ConstGc` above covers the literal. These cover `Inst::Materialize`,
// which is the hot allocating instruction: a loop counter, an accumulator, a
// fused-pipeline sink. The IR-shape tests in `lower.rs` say the sequence is
// emitted; these say it computes the right thing, which is the half an
// off-by-one in the index arithmetic breaks — silently, as a wrong number.
// ===========================================================================

/// A value *computed* into the interned range is the runtime's own object, at
/// every boundary of the table.
///
/// The values are reached through a loop so no folding can turn them back into
/// literals: what is under test is the inline probe's arithmetic — the subtract
/// by `SMALL_INT_MIN`, the unsigned span compare and the scaled load — not
/// `Inst::ConstGc`, which is already covered above and takes a different path
/// entirely (a compile-time index, no pacing test, no branch).
///
/// The boundaries are the whole point. An index computed as `v - MIN + 1` reads
/// the neighbouring slot and answers a number one too large for every value in
/// the language; an inclusive/exclusive slip at the top reads one past the end
/// of the table. Both are wrong *answers* rather than crashes, which is the only
/// place in ADR-113 that failure mode appears at all.
#[test]
fn an_inline_interned_box_is_the_object_the_wrapper_would_have_answered() {
    use praxis_runtime::{SMALL_INT_MAX, SMALL_INT_MIN};

    for v in [
        SMALL_INT_MIN,
        SMALL_INT_MIN + 1,
        -1,
        0,
        1,
        SMALL_INT_MAX - 1,
        SMALL_INT_MAX,
    ] {
        let src = format!(
            "fn main() -> Int {{\n  var x = 0\n  var i = 0\n  \
             while i < 1 {{ x = x + ({v})\n i = i + 1 }}\n  x\n}}\n"
        );
        let (rt, result) = run_main(&src);
        assert!(!rt.has_pending_fault(), "faulted at {v}: {:?}", rt.fault());
        assert_eq!(
            result.as_int(),
            v,
            "the inline probe answered the wrong value"
        );
        let interned = rt
            .immortals()
            .small_int(v)
            .expect("the boundary values are in the table by construction");
        assert_eq!(
            result.as_ptr(),
            interned.as_ptr(),
            "a computed {v} must be the *same object* `praxis_alloc_int` would \
             have answered — the inline path indexes the table generated code \
             was handed, and an off-by-one here is a wrong number rather than a \
             crash"
        );
    }

    // And one step outside the range on each side is a fresh object, which is
    // what says the span compare is a bound rather than decoration.
    for v in [SMALL_INT_MIN - 1, SMALL_INT_MAX + 1] {
        let src = format!(
            "fn main() -> Int {{\n  var x = 0\n  var i = 0\n  \
             while i < 1 {{ x = x + ({v})\n i = i + 1 }}\n  x\n}}\n"
        );
        let (rt, result) = run_main(&src);
        assert!(!rt.has_pending_fault(), "faulted at {v}: {:?}", rt.fault());
        assert_eq!(result.as_int(), v);
        assert!(
            rt.immortals().small_int(v).is_none(),
            "{v} is outside the table, so the wrapper allocated it"
        );
    }
}

/// **The collector still runs when the inline path is the only allocator.**
///
/// This is ADR-113's obligation observed from the outside. The inline sequence
/// hands off to `praxis_alloc_int` on two branches — the value is out of range,
/// or `Heap::collection_is_due` — and the wrapper paces through `Heap::pace`
/// exactly as it always did. A version that dropped the pacing test would still
/// pass every other test in this file: the answers are identical, and the
/// collector's absence shows up only as a heap that keeps growing.
///
/// Deliberately **not** an `after < before + 1` shape. The loop allocates
/// something like 40,000 objects that nothing retains; asserting the live count
/// ends up below the *iteration* count cannot be satisfied without a collection
/// having run, whatever the pacer's ladder happens to be on this machine.
#[test]
fn a_loop_that_boxes_only_large_ints_still_collects() {
    const ITERATIONS: i64 = 20_000;

    // Both `s` and `i` leave `small_int`'s range almost immediately, so every
    // iteration is two real allocations on the cold path.
    let src = format!(
        "fn main() -> Int {{\n  var i = 0\n  var s = 0\n  \
         while i < {ITERATIONS} {{ s = s + 100000\n i = i + 1 }}\n  i\n}}\n"
    );
    let (rt, result) = run_main(&src);
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), ITERATIONS);

    let live = rt.heap().stats().live_count;
    assert!(
        (live as i64) < ITERATIONS,
        "the loop allocated about {} objects and retains none of them, so a \
         heap holding {live} of them means no collection ran — the inline \
         `Materialize` path skipped the pacing test, or its cold path stopped \
         reaching `Heap::pace` (ADR-113, ADR-040)",
        2 * ITERATIONS
    );
}

// ===========================================================================
// ADR-119 — generated code claims the block itself.
//
// The IR tests in `lower.rs` are the gate on the three parts of decision 1,
// because all three are claims about the instruction stream. What a running
// program *can* see is the arithmetic: whether the object the sequence built is
// the object the wrapper would have built, whether the heap counted it, and
// whether the collector can still reclaim it. These are those.
// ===========================================================================

/// Every block the inline claim takes is counted by the heap exactly once.
///
/// **This is the half of decision 1 part 3 a program can see, and the failure it
/// prevents is not a wrong number.** `Heap::live_count` is only ever
/// *decremented* — `sweep` takes it down by the blocks it reclaimed and never
/// recomputes it — so a claim that skipped the bump does not leave the statistic
/// low, it underflows a `usize` on the first collection. The same holds one
/// level down for `PageHeader::live_count`, where the consequence is worse:
/// `relink_pages` reads it to decide which availability list a page joins, so an
/// understated page joins the *empty* pool and `reclass` hands its storage —
/// blocks with live objects in them — to another layout.
///
/// The count is taken as a difference between two runs of the same program with
/// different bounds, so the input `Text`, the frame metadata and every other
/// fixed cost cancels and the assertion can be an exact equality rather than an
/// inequality that would pass with the bump missing.
#[test]
fn the_inline_claim_counts_every_block_it_takes() {
    const BOXES: i64 = 200;

    // Each iteration boxes one value the intern table does not hold, and the
    // `Vec` retains all of them, so nothing here is reclaimable and the count is
    // the number of claims. `i` itself stays inside `small_int`'s range and
    // costs nothing, which is what makes the difference exactly `BOXES`.
    let live_after = |iterations: i64| {
        let src = format!(
            "fn main() -> Int {{\n  var v = Vec[Int]()\n  var i = 0\n  \
             while i < {iterations} {{ v.push(i * 100000 + 100000)\n i = i + 1 }}\n  \
             v.len()\n}}\n"
        );
        let (rt, result) = run_main(&src);
        assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
        assert_eq!(result.as_int(), iterations);
        rt.heap().stats().live_count
    };

    assert_eq!(
        live_after(BOXES) as i64 - live_after(0) as i64,
        BOXES,
        "an inline claim must bump `Heap::live_count` exactly as \
         `Heap::alloc_raw` does — see ADR-119 decision 1 part 3"
    );
}

/// The object the inline claim builds is the object the wrapper would have
/// built: same value, same type, and it survives a collection.
///
/// The header is three words generated code writes itself, and each has a
/// distinct failure. A wrong descriptor makes `ExtractScalar`'s inline proof
/// fail and route to `praxis_int_load`, which aborts. A wrong recorded payload
/// displacement reads the wrong eight bytes. A wrong `heap_id` is the quiet one:
/// the mark phase compares it against the heap's own *before* it masks the
/// address to find the page (ADR-039 decision 2), so an object stamped with the
/// wrong id is not traced — it is simply reclaimed underneath a live reference.
/// That is why this retains its values across a collection rather than reading
/// them back immediately.
#[test]
fn an_inline_claimed_object_is_what_the_wrapper_would_have_answered() {
    // 40,000 boxes against a 64 KiB initial threshold at 24 bytes a block: the
    // collector runs many times over, and every one of these is rooted through
    // the `Vec` the whole time.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var v = Vec[Int]()\n  var i = 0\n  \
         while i < 40000 { v.push(i * 7 + 100000)\n i = i + 1 }\n  \
         var sum = 0\n  var j = 0\n  \
         while j < 40000 { sum = sum + v[j] - (j * 7 + 100000)\n j = j + 1 }\n  \
         sum\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(
        result.as_int(),
        0,
        "every value read back must be the value stored — a claim that wrote \
         the payload at the wrong displacement, or that handed out a block the \
         collector had not really freed, shows up here and nowhere else"
    );
}

/// A loop that boxes only `Float`s still collects.
///
/// `a_loop_that_boxes_only_large_ints_still_collects` is ADR-113's version of
/// this. For `Float` the pacing compare is emitted inline (ADR-119); dropping it
/// would let this loop allocate 40,000 blocks and never offer the collector a
/// turn — which is exactly what ADR-040's token exists to make unwritable.
#[test]
fn a_loop_that_boxes_only_floats_still_collects() {
    const ITERATIONS: i64 = 20_000;

    let src = format!(
        "fn main() -> Int {{\n  var x = 0.0\n  var i = 0\n  \
         while i < {ITERATIONS} {{ x = x + 1.5\n i = i + 1 }}\n  i\n}}\n"
    );
    let (rt, result) = run_main(&src);
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), ITERATIONS);

    let live = rt.heap().stats().live_count;
    assert!(
        (live as i64) < ITERATIONS,
        "the loop boxed {ITERATIONS} `Float`s and retains one, so a heap \
         holding {live} of them means no collection ran — the inline claim \
         skipped the pacing test it is the first `Float` path ever to have \
         (ADR-119, ADR-040)"
    );
}

/// A `Bool` or `Unit` literal answers the runtime's own singleton, and does so
/// without a call or an allocation.
///
/// Both are immortals — `praxis_alloc_bool` and `praxis_alloc_unit` return the
/// cached references and their manifest rows say `Effect::Pure` — so lowering
/// them as `Inst::ConstGc` rather than `Inst::Alloc` costs neither an extern
/// call nor the full shadow-frame spill `liveness::is_gc_safepoint` forces at an
/// `Alloc`, at a point where no collection can happen. This pins the object
/// identity that makes that folding a no-op semantically.
#[test]
fn a_bool_or_unit_literal_is_the_runtime_singleton_and_allocates_nothing() {
    let (control, _) = run_main("fn main() -> Int {\n  0\n}\n");
    let floor = control.heap().stats().live_count;

    // A hundred `true`s and a hundred `false`s: one object each, no allocation.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var i = 0\n  var n = 0\n  \
         while i < 100 { var t = true\n var f = false\n \
         if t { n = n + 1 } else { n = n }\n \
         if f { n = n } else { n = n + 1 }\n i = i + 1 }\n  n\n}\n",
    );
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(result.as_int(), 200);
    assert_eq!(
        rt.heap().stats().live_count,
        floor,
        "Bool literals must not allocate"
    );

    // And the value a `Bool` literal produces is the runtime's own singleton,
    // not some other object that happens to read as `true`.
    let (rt, result) = run_main("fn main() -> Bool {\n  true\n}\n");
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(
        result.as_ptr(),
        rt.immortals().true_().as_ptr(),
        "a `true` literal is the immortal `true`"
    );

    let (rt, result) = run_main("fn main() -> Unit {\n  var x = 1\n}\n");
    assert!(!rt.has_pending_fault(), "faulted: {:?}", rt.fault());
    assert_eq!(
        result.as_ptr(),
        rt.immortals().unit().as_ptr(),
        "a Unit tail is the immortal `Unit`"
    );
}

/// An interned literal bound to a local is still renderable at a fault.
///
/// `Inst::ConstGc` is not a debug point, so the temp holding an interned literal
/// is *not* spilled into the debug frame at the instruction that defines it. It
/// is spilled at the next `CheckFault`, whose `DebugSlots` is over-approximate
/// on purpose and includes every `Gc` local defined so far in the block — and
/// the verifier guarantees a `CheckFault` immediately precedes every fault
/// diversion. So a crash snapshot must still render the value, not `<uninit>`.
#[test]
fn a_crash_snapshot_still_shows_an_interned_literal_bound_to_a_local() {
    // `n` is 7 — interned, so its lowering is a `ConstGc` and nothing spills it
    // at that point. The division then faults, and the snapshot must show `n`.
    let src = "fn main() -> Int {\n  var n = 7\n  var z = 0\n  n / z\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault(), "the division must fault");
    let snap = rt.crash_snapshot().expect("snapshot captured");
    let n = snap.frames[0]
        .locals
        .iter()
        .find(|l| l.name() == "n")
        .expect("`n` is a named local of the faulting frame");
    let value = n
        .value
        .expect("an interned literal must not read back as <uninit>");
    assert_eq!(
        value.as_int(),
        7,
        "and it must be the value the source wrote"
    );
}

// ADR-101 — the shadow stack is one contiguous region.
//
// A frame is a run of slots claimed by bumping a pointer, not an allocation, so
// the property is "every epilogue restores the `top` its prologue saved" rather
// than "every push has a matching free". That one is observable: the stack is
// empty again when a run ends. These tests assert it on the three ways
// a run can end — a normal return, a fault, and the recursion-depth guard —
// because an unbalanced prologue would otherwise be a slow leak that surfaces
// only as a wrong root set thousands of calls later.
// ===========================================================================

#[test]
fn adr100_a_clean_run_leaves_the_shadow_stack_empty() {
    // Recursion, allocation and collection, then a balanced unwind: 200 nested
    // frames each holding a live `Vec` across the recursive call.
    let src = "fn build(n: Int) -> Vec[Int] {\n  \
               if n == 0 { Vec() } else { var v = build(n - 1)\n v.push(n)\n v }\n}\n\
               fn main() -> Int { build(200).len() }\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 200);
    assert!(
        rt.shadow_stack().is_empty(),
        "{} slots are still claimed after a clean run",
        rt.shadow_stack().len()
    );
}

#[test]
fn adr100_a_fault_epilogue_restores_the_shadow_stack() {
    // The fault path is the one that would leak frames if its epilogue were
    // wrong, because it unwinds through every caller at once. An
    // IndexOutOfBounds three calls deep must leave the stack exactly as it was
    // before `main` was entered.
    let src = "fn inner(v: Vec[Int]) -> Int { v[5] }\n\
               fn middle(v: Vec[Int]) -> Int { inner(v) }\n\
               fn main() -> Int {\n  var v = Vec()\n  v.push(1)\n  middle(v)\n}\n";
    let (rt, _result) = run_main(src);
    assert!(rt.has_pending_fault());
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::IndexOutOfBounds);
    assert!(
        rt.shadow_stack().is_empty(),
        "{} slots are still claimed after a fault unwind",
        rt.shadow_stack().len()
    );
}

#[test]
fn adr100_a_stack_overflow_restores_the_shadow_stack() {
    // 8000 frames, each holding a live `Vec`, unwound by the depth guard. The
    // over-limit path is the one block in a generated function whose `return_`
    // is not preceded by an epilogue — because the guard runs before the push,
    // so that path claimed nothing. Wrong in either direction and the stack
    // ends non-empty (nothing popped) or below its base (popped twice), the
    // second of which trips `SlotStackHeader::live_slots`' debug assertion.
    let src = "fn count(n: Int) -> Int {\n  var v = Vec()\n  v.push(n)\n  \
               count(n + 1) + v.len()\n}\n\
               fn main() -> Int { count(0) }\n";
    let (rt, _result) = run_main(src);
    assert_eq!(rt.fault(), praxis_runtime::FaultKind::StackOverflow);
    assert!(
        rt.shadow_stack().is_empty(),
        "{} slots are still claimed after a stack-overflow unwind",
        rt.shadow_stack().len()
    );
}

#[test]
fn adr100_a_wide_frame_recursing_deep_claims_and_gives_back_every_slot() {
    // The empirical half of the capacity argument, as far as it can be taken.
    // `SHADOW_STACK_SLOTS` is sized from the *byte* budget, on the strength of
    // two premises — every prologue guards before it pushes, and every frame is
    // a `SlotCount` — and nothing checks the limit at run time because of it.
    // The failure mode that argument rules out is therefore silent: a frame
    // wide enough and a recursion deep enough to walk `top` past the end of the
    // reservation.
    //
    // No test can reach that corner. A frame spends `FRAME_BYTES_PER_SLOT` on
    // every slot past `REFERENCE_FRAME_SLOTS`, so the claimed slots of all live
    // frames sum to at most `STACK_BUDGET_BYTES / FRAME_BYTES_PER_SLOT +
    // MAX_RECURSION_DEPTH × REFERENCE_FRAME_SLOTS`, which is what the
    // reservation covers (ADR-105). Sizing it as MAX_RECURSION_DEPTH ×
    // MAX_SHADOW_SLOTS instead would multiply the deepest recursion by the
    // widest frame as if a program could have both at once: it cannot, and the
    // guard is what says so.
    //
    // What this test shows is the mechanism working at a width and depth an
    // actual program could reach. 600 frames of twenty collections is well
    // inside the budget (which covers roughly 1990 of them).
    //
    // What the width buys is the other half of this test: twenty collections
    // live across the recursive call is twenty *co-live* roots, so ADR-128
    // decision 2's colouring cannot fold them into fewer slots, and the frame is
    // genuinely wide on both stacks rather than only on the debugger's.
    let mut body = String::from("fn wide(n: Int) -> Int {\n  if n == 0 { return 0 }\n");
    for i in 0..20 {
        body.push_str(&format!("  var v{i} = Vec()\n  v{i}.push(n)\n"));
    }
    body.push_str("  var s = wide(n - 1)\n");
    for i in 0..20 {
        body.push_str(&format!("  s = s + v{i}.len()\n"));
    }
    body.push_str("  s\n}\n");

    // Every frame's twenty collections are live across the recursive call, so
    // the answer counts them all: a slot zeroed but not spilled, or a frame
    // whose base overlapped its callee's, loses one.
    let (rt, result) = run_main(&format!("{body}fn main() -> Int {{ wide(600) }}"));
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(
        result.as_int(),
        600 * 20,
        "each frame's twenty Vecs survive"
    );
    assert!(
        rt.shadow_stack().is_empty(),
        "{} slots are still claimed",
        rt.shadow_stack().len()
    );
}

/// ADR-105: the guard charges bytes, so a wide frame runs out of budget at a
/// depth a reference frame sails past.
///
/// Stated as a differential, which is the only way to state it without asserting
/// a number about the host's stack. Both halves recurse to the *same* depth and
/// differ only in how wide their frames are — exactly the difference a call
/// count cannot see.
///
/// The wide half's overflow is not asserted directly, because asserting it means
/// deliberately overflowing this process: a twenty-two-collection frame measures
/// 294 bytes, so 8000 such calls want 2.35 MiB, and every test here runs on
/// std's 2 MiB default (libtest passes no `stack_size`).
#[test]
fn adr105_a_wide_frame_faults_where_a_reference_frame_does_not() {
    const DEPTH: u32 = 5000;

    // Twenty-two collections, all live across the recursive call so none can be
    // sunk. Its `frame_cost` is several times the reference frame's, so the one
    // budget buys it far fewer frames.
    let mut body = String::from("fn wide(n: Int) -> Int {\n  if n == 0 { return 0 }\n");
    for i in 0..22 {
        body.push_str(&format!("  var v{i} = Vec()\n  v{i}.push(n)\n"));
    }
    body.push_str("  var s = wide(n - 1)\n");
    for i in 0..22 {
        body.push_str(&format!("  s = s + v{i}.len()\n"));
    }
    body.push_str("  s\n}\n");

    let (wide, _) = run_main(&format!("{body}fn main() -> Int {{ wide({DEPTH}) }}"));
    assert_eq!(
        wide.fault(),
        praxis_runtime::FaultKind::StackOverflow,
        "{DEPTH} frames of twenty-two collections must fault, not abort the host"
    );
    // Guard-first means the frame that was refused pushed nothing, and every
    // frame below it unwound through its own epilogue.
    assert!(
        wide.shadow_stack().is_empty(),
        "{} slots are still claimed after the budget ran out",
        wide.shadow_stack().len()
    );

    // The control: the same depth, a reference-width frame, no fault. Under a
    // call count these two are indistinguishable.
    let (narrow, result) = run_main(&format!(
        "fn count(n: Int) -> Int {{ if n == 0 {{ 0 }} else {{ 1 + count(n - 1) }} }}\n\
         fn main() -> Int {{ count({DEPTH}) }}\n"
    ));
    assert!(
        !narrow.has_pending_fault(),
        "the same depth in a narrow frame must still pass: {:?}",
        narrow.fault()
    );
    assert_eq!(result.as_int(), i64::from(DEPTH));
}

/// ADR-105: the budget is spent by width, so the depth a program reaches is a
/// function of its frames — and an ordinary one is unaffected by this change.
///
/// The pair is the point. Both programs recurse deep; they differ only in how
/// wide their frames are. This one succeeds and the wide one faults — a call
/// count cannot tell them apart, which is precisely why it is the wrong
/// quantity.
///
/// **This is also the gate on `REFERENCE_FRAME_SLOTS`.** `count` is the exact
/// program `MAX_RECURSION_DEPTH` was chosen for, and the budget is derived so
/// that it reaches that depth and no less. If a codegen change makes `count`
/// wider than eleven `Gc` locals, this test fails — and the fix is to re-measure
/// that constant, not to lower the depth asserted here.
#[test]
fn adr105_a_reference_frame_still_recurses_as_deep_as_the_call_count_allowed() {
    // `count(d)` makes `d + 1` frames of its own, under `main`'s. Two off the
    // constant is the arithmetic of the call chain, not slack in the budget.
    let deep = praxis_runtime::MAX_RECURSION_DEPTH - 2;
    let src = format!(
        "\
fn count(n: Int) -> Int {{ if n == 0 {{ 0 }} else {{ 1 + count(n - 1) }} }}
fn main() -> Int {{ count({deep}) }}
"
    );
    let (rt, result) = run_main(&src);
    assert!(
        !rt.has_pending_fault(),
        "the reference frame lost depth to ADR-105's byte budget: {:?}",
        rt.fault()
    );
    assert_eq!(result.as_int(), i64::from(deep));
}

#[test]
fn adr100_a_praxis_closure_called_from_a_graph_helper_balances_the_shadow_stack() {
    // Re-entrancy. `praxis_bfs` calls a Praxis closure from inside an
    // `abi_guard!` and inside a `NativeScope`, so generated prologues run
    // underneath native runtime code that is itself holding roots. The
    // discipline is still strictly LIFO — the closure's own epilogue restores
    // both `top` and `stack_left` — and the native chain and the shadow
    // stack stay independent arms of `RuntimeRoots`.
    //
    // The closure recurses and allocates, so it pushes frames of its own and
    // forces collections while the helper's own intermediates are half-built.
    let src = "fn chain(n: Int, k: Int) -> Vec[Int] {\n  \
               var v = Vec()\n  \
               if k == 0 {\n    if n < 60 { v.push(n + 1) }\n    return v\n  }\n  \
               var inner = chain(n, k - 1)\n  \
               for x in inner { v.push(x) }\n  \
               v\n}\n\
               fn main() -> Int { bfs(0, |n| chain(n, 8)).len() }\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 61, "0..=60 reached, each once");
    assert!(
        rt.shadow_stack().is_empty(),
        "{} slots are still claimed after a re-entrant walk",
        rt.shadow_stack().len()
    );
}

// ===========================================================================
// ADR-125: every binding is a binding, and the compiler decides its storage.
//
// Every binding is assignable: a parameter, a `for` variable, a match arm's
// binding and a name a pattern introduces. Each has its own binding site in the
// MIR builder, so each is a distinct way for a write to land in the wrong place.
// These tests are one per site, and they run rather than type-check because what
// is being asserted is where the value went.
// ===========================================================================

#[test]
fn a_captured_and_assigned_parameter_shares_one_cell() {
    // The write is in the closure and the read is in the frame that owns the
    // parameter, so a copy would answer 1. Only a `VarCell` allocated in the
    // prologue — the parameter's binding site — answers 16.
    let src = "fn bump(n: Int) -> Int {\n  \
               var add = |k| { n = n + k }\n  \
               add(10)\n  add(5)\n  n\n}\n\
               fn main() -> Int { bump(1) }\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 16, "both writes reached the parameter");
}

#[test]
fn a_for_variable_can_be_assigned_within_its_step() {
    // The clamp writes the loop variable, and the next step overwrites it from
    // the iterator — so the write must last exactly one iteration.
    let src = "fn main() -> Int {\n  \
               var v = Vec()\n  v.push(1)\n  v.push(50)\n  v.push(3)\n  \
               var total = 0\n  \
               for x in v {\n    if x > 10 { x = 10 }\n    total += x\n  }\n  \
               total\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 14, "1 + 10 + 3");
}

#[test]
fn each_step_of_a_for_captures_its_own_cell() {
    // Each iteration of a `for` is a *fresh* binding, so three closures made in
    // three steps must hold three values. One cell hoisted out of the loop —
    // the obvious way to write the boxing — would answer 200 three times.
    let src = "fn main() -> Int {\n  \
               var fs = Vec()\n  \
               for i in 0..3 {\n    i = i * 100\n    fs.push(|_| i)\n  }\n  \
               fs.get(0)(0) + fs.get(1)(0) * 10 + fs.get(2)(0) * 100\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    // 0 + 100·10 + 200·100 — positional, so a shared cell (200 in all three)
    // would read 222000 and not merely a wrong total.
    assert_eq!(result.as_int(), 21000, "steps 0, 100, 200 in that order");
}

#[test]
fn assigning_a_match_binding_does_not_write_the_scrutinee() {
    // A match arm's binding must not *alias* the scrutinee's local: for a plain
    // `match v` that local is `v`'s own, so the write would land in `v`. The arm
    // binds a slot of its own precisely when something writes it.
    let src = "fn main() -> Int {\n  \
               var v = 7\n  \
               var got = match v { n => { n = 99\n n } }\n  \
               got * 100 + v\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 9907, "the arm saw 99 and `v` is still 7");
}

#[test]
fn a_captured_and_assigned_destructured_name_shares_one_cell() {
    // The same property as the parameter's, at the fifth binding site: a name a
    // destructuring pattern introduces, boxed where the component is bound.
    let src = "fn main() -> Int {\n  \
               var g = |(a, b)| {\n    var h = |k| { a = a + k }\n    h(10)\n    a + b\n  }\n  \
               g((1, 2))\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 13, "11 + 2");
}

#[test]
fn a_binding_nothing_writes_is_captured_by_value() {
    // The complement of the four above, and the reason `reassigned` is a fact
    // and not a keyword: a capture nothing writes needs no cell.
    let src = "fn main() -> Int {\n  \
               var base = 10\n  \
               var f = |k| k + base\n  \
               f(1) + f(2)\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 23);
}

/// **ADR-136, the runtime half.** `Text.int()` answers the number the text
/// spells, and `None` for every text that spells none.
///
/// The signature is `Option[Int]` and not `Int` for §4.7's reason — a text that
/// is not a number is absence, not a fault — so the rejections below are values
/// the program can take apart, not crashes.
///
/// **The accepted set is §7.4's `int` atomic** over the whole trimmed text, so
/// `t.int()` and `parse(t, int)` cannot disagree. `"+5"` is in the rejected list
/// for that reason and no other: `i64::from_str` takes it and the atomic does
/// not.
#[test]
fn text_int_parses_a_number_or_answers_none() {
    // The accepted spelling, including the surrounding space a line read off
    // input carries.
    for (text, want) in [("12", 12), ("  42 ", 42), ("-7", -7), ("0", 0)] {
        let src = format!(
            "fn main() -> Int {{\n  match \"{text}\".int() {{ Some(n) => n, None => -999 }}\n}}\n"
        );
        let (rt, result) = run_main(&src);
        assert!(
            !rt.has_pending_fault(),
            "`{text}` faulted: {:?}",
            rt.fault()
        );
        assert_eq!(result.as_int(), want, "`{text}`");
    }

    // Everything else is `None`, and none of it faults. `"99…9"` is `Y013`'s
    // rule at run time — a value outside `Int` is not a saturated answer — and
    // `"+5"`, `"12abc"` and `"12.5"` are each a text `parse(t, int)` refuses.
    for text in [
        "abc",
        "",
        "1 2",
        "12abc",
        "+5",
        "0x10",
        "1_000",
        "12.5",
        "99999999999999999999999",
    ] {
        let src = format!(
            "fn main() -> Int {{\n  match \"{text}\".int() {{ Some(n) => n, None => -999 }}\n}}\n"
        );
        let (rt, result) = run_main(&src);
        assert!(
            !rt.has_pending_fault(),
            "`{text}` faulted: {:?}",
            rt.fault()
        );
        assert_eq!(result.as_int(), -999, "`{text}` spells no Int");
    }
}

/// **ADR-136's twin.** `Text.float()` reads §7.4's `float` atomic over the whole
/// trimmed text, and answers `None` for everything else.
///
/// The rejections are the interesting half, and each names a way this could have
/// been written to disagree with `parse(t, float)`: `f64::from_str` accepts
/// `"inf"`, `"infinity"`, `"nan"` and `"1."`, and §7.4's `float` accepts none of
/// them. `Float` still has infinities and NaN — `1.0 / 0.0` is one, and
/// `Float.to_text()` prints them — what has no spelling is reading one back out
/// of arbitrary text.
///
/// Compared as *text* rather than as a number: an equality test against a
/// `Float` would pass for a `None` mapped to `0.0`.
#[test]
fn text_float_parses_a_number_or_answers_none() {
    for (text, want) in [
        ("1.5", "1.5"),
        ("  -2 ", "-2.0"),
        ("+5.0", "5.0"),
        ("1e10", "10000000000.0"),
        ("3", "3.0"),
        ("0.0", "0.0"),
    ] {
        let src = format!(
            "fn main() -> Text {{\n  match \"{text}\".float() {{ Some(x) => x.to_text(), None => \"none\" }}\n}}\n"
        );
        let (rt, result) = run_main(&src);
        assert!(
            !rt.has_pending_fault(),
            "`{text}` faulted: {:?}",
            rt.fault()
        );
        assert_eq!(result.as_text(), want, "`{text}`");
    }

    for text in [
        "1.", "1e", "inf", "infinity", "nan", "abc", "", "1 2", "1.5x",
    ] {
        let src = format!(
            "fn main() -> Text {{\n  match \"{text}\".float() {{ Some(x) => x.to_text(), None => \"none\" }}\n}}\n"
        );
        let (rt, result) = run_main(&src);
        assert!(
            !rt.has_pending_fault(),
            "`{text}` faulted: {:?}",
            rt.fault()
        );
        assert_eq!(result.as_text(), "none", "`{text}` spells no Float");
    }
}

/// **ADR-143.** `Int.to_text()` is the digits `out` prints, through the real
/// JIT and the real wrapper.
///
/// `i64::MIN` is spelled `-9223372036854775807 - 1` because `-9223372036854775808`
/// is a negation of a literal one past `i64::MAX`. It is in the list because a
/// renderer that negates before formatting overflows on exactly that value.
#[test]
fn int_to_text_is_the_digits_out_prints() {
    for (expr, want) in [
        ("(1660)", "1660"),
        ("(0)", "0"),
        ("(-7)", "-7"),
        ("(9223372036854775807)", "9223372036854775807"),
        ("(-9223372036854775807 - 1)", "-9223372036854775808"),
    ] {
        let src = format!("fn main() -> Text {{\n  {expr}.to_text()\n}}\n");
        let (rt, result) = run_main(&src);
        assert!(!rt.has_pending_fault(), "{expr} faulted: {:?}", rt.fault());
        assert_eq!(result.as_text(), want, "{expr}");
    }
}

/// **ADR-143.** A labelled debug line is one call.
///
/// The row composes with ADR-085's `+`, and composing is a separate claim from
/// existing — `+` refuses to stringify, so the conversion has to be explicit and
/// has to land on a `Text`.
#[test]
fn a_labelled_line_is_one_call() {
    let src = "fn main() -> Text {\n  \"splits: \" + (1660).to_text()\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_text(), "splits: 1660");

    // And the whole family concatenates, which is the point of closing it: a
    // line built from a number, a character and a float needs no third spelling.
    let src = "fn main() -> Text {\n  (3).to_text() + \"#\"[0].to_text() + (1.5).to_text()\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_text(), "3#1.5");
}

/// **ADR-143.** `Char.to_text()` is the character `out` prints, including one
/// that does not fit a byte.
///
/// The multi-byte case is the four-byte payload read observed from a program:
/// `"héllo"[1]` is `é`, and a wrapper that took eight bytes for it would answer
/// something else entirely.
#[test]
fn char_to_text_is_the_character_out_prints() {
    for (expr, want) in [
        ("\"#\"[0]", "#"),
        ("\"héllo\"[1]", "é"),
        ("(233).to_char()", "é"),
        ("(9731).to_char()", "☃"),
    ] {
        let src = format!("fn main() -> Text {{\n  {expr}.to_text()\n}}\n");
        let (rt, result) = run_main(&src);
        assert!(!rt.has_pending_fault(), "{expr} faulted: {:?}", rt.fault());
        assert_eq!(result.as_text(), want, "{expr}");
    }
}

/// **ADR-144.** `join` puts the separator between the elements and nowhere
/// else, over a `Vec` and over a receiver that is not one.
///
/// The `Set` case is the generic receiver earning its keep: `join` is one row on
/// `Iterable`, so the materializing walk in front of the wrapper is what makes a
/// non-`Vec` source work at all (ADR-127 decision 3).
#[test]
fn join_puts_the_separator_between_the_items() {
    let src = "fn main() -> Text {\n  var v = Vec[Text]()\n  v.push(\"a\")\n  v.push(\"b\")\n  \
               v.push(\"c\")\n  v.join(\", \")\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_text(), "a, b, c");

    // An empty separator is the no-separator spelling, and it is written rather
    // than defaulted — the catalog has no optional arguments.
    let src = "fn main() -> Text {\n  [\"a\", \"b\", \"c\"].join(\"\")\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_text(), "abc");

    // An empty sequence answers the empty Text, not a stray separator.
    let src = "fn main() -> Int {\n  var v = Vec[Text]()\n  v.join(\"-\").len()\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);

    // A `Set` receiver, materialized in front of the wrapper. One member, so the
    // answer does not depend on the snapshot's order.
    let src = "fn main() -> Text {\n  var s = Set()\n  s.insert(\"only\")\n  s.join(\",\")\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_text(), "only");

    // A sequence of numbers joins by rendering first, which is the composition
    // ADR-143 and ADR-144 were decided together to make available. `join` itself
    // still refuses to render.
    let src = "fn main() -> Text {\n  [1, 2, 3].map(|n| n.to_text()).join(\"-\")\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_text(), "1-2-3");
}

/// **ADR-144.** A `Grid` row renders back as the line it was read from.
///
/// `out(g.row(y))` prints `[., ., |]`; `to_text()` is what spells the line
/// itself, which is how a grid puzzle is debugged. The round trip is the
/// assertion: the text goes in through `read grid(char)` and comes back out
/// unchanged.
#[test]
fn a_grid_row_renders_back_as_a_line() {
    let src = "fn main() -> Text {\n  var g = read grid(char)\n  g.row(1).to_text()\n}";
    let (rt, result) = run_main_with_input(src, "..|\n#.#\n");
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_text(), "#.#");

    // The empty sequence, and a character that does not fit a byte.
    let src = "fn main() -> Int {\n  var v = Vec[Char]()\n  v.to_text().len()\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);

    let src = "fn main() -> Text {\n  var v = Vec[Char]()\n  v.push(\"é\"[0])\n  \
               v.push(\"☃\"[0])\n  v.to_text()\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_text(), "é☃");
}

/// **ADR-145.** A countdown is a reversed range: `43210` is what a manual
/// `while y >= sy { … ; y = y - 1 }` loop would otherwise have to spell.
#[test]
fn a_countdown_is_a_reversed_range() {
    let src =
        "fn main() -> Int {\n  var t = 0\n  for y in (0..5).reversed() { t = t * 10 + y }\n  t\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 43210);

    // ADR-059 decision 3: `5..0` is an empty range and says nothing. The
    // countdown has a spelling; the descending literal is not an error.
    let src = "fn main() -> Int {\n  var t = 0\n  for y in 5..0 { t = t + 1 }\n  t\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(
        result.as_int(),
        0,
        "a descending range is empty, not reversed"
    );

    let src = "fn main() -> Int {\n  (0..0).reversed().len()\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

/// **ADR-145.** `reversed` answers a new `Vec`, leaves the receiver alone, and
/// carries no capability bound — which is what separates it from the barrier it
/// sits beside.
///
/// `32` is `v.get(0)` still 3 and `r.get(0)` already 2. The closure case is the
/// two-sided half: the same receiver earns `Y006` from `sorted()`, because
/// ordering asks for `compare` and reversal asks for nothing.
#[test]
fn a_reversed_vec_is_new_and_needs_nothing_of_its_elements() {
    let src = "fn main() -> Int {\n  var v = [3, 1, 2]\n  var r = v.reversed()\n  \
               v.get(0) * 10 + r.get(0)\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 32, "the source Vec is not mutated");

    // Every receiver, through its own accessor rather than through
    // `praxis_vec_get` on a foreign payload (ADR-127 decision 2).
    let src = "fn main() -> Char {\n  \"abc\".reversed().get(0)\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_char(), 'c');

    let src = "fn main() -> Int {\n  var d = Deque()\n  d.push_back(1)\n  d.push_back(2)\n  \
               d.reversed().get(0)\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);

    // A `Vec` of closures has no `compare` and no `hash`, so `sorted()` and
    // `unique()` refuse it at check time. `reversed()` does not ask.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(|x| x + 1)\n  \
               v.push(|x| x + 2)\n  v.reversed().len()\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

// --- ADR-149: the two groupings ---------------------------------------------

/// **ADR-149.** `chunks(n)` partitions: every element once, in order, and a
/// length the size does not divide leaves a short last chunk.
///
/// `325` is three chunks, the first holding two elements and the last holding
/// one — the tail neither dropped nor padded.
#[test]
fn chunks_partitions_and_keeps_a_short_last_chunk() {
    let src = "fn main() -> Int {\n  var c = [1, 2, 3, 4, 5].chunks(2)\n  \
               c.count() * 100 + c.get(0).count() * 10 + c.get(2).count()\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 321);

    // The elements themselves, read back through the nested subscript — which
    // is the half a count cannot see.
    let src = "fn main() -> Int {\n  var c = [1, 2, 3, 4, 5].chunks(2)\n  \
               c[1][0] * 10 + c[1][1]\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 34);

    // A size at or above the length is one chunk, not a fault and not one
    // chunk per element.
    let src = "fn main() -> Int {\n  [1, 2, 3].chunks(9).count() * 10 + \
               [1, 2, 3].chunks(9).get(0).count()\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 13);

    // …and an empty receiver is an empty answer, at any size.
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.chunks(2).count()\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 0);
}

/// **ADR-149.** `windows(n)` slides by one and keeps only the runs that fit, so
/// a receiver shorter than the size answers `[]` — the one place the two
/// groupings differ, and the one worth a test of its own.
///
/// The sums `[3, 5, 7]` are what the shape is *for*: "compare each element with
/// its neighbour" has a spelling that does not index.
#[test]
fn windows_slide_by_one_and_drop_a_run_that_does_not_fit() {
    let src = "fn main() -> Int {\n  var w = [1, 2, 3, 4].windows(2)\n  \
               w.count() * 100 + w[0][1] * 10 + w[2][0]\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 323);

    // Exactly the length is one window; one past it is none. Off by one here is
    // the difference between the empty answer and a wrong one.
    let src = "fn main() -> Int {\n  [1, 2, 3].windows(3).count() * 10 + \
               [1, 2, 3].windows(4).count()\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(
        result.as_int(),
        10,
        "a run that does not fit is dropped, not faulted"
    );

    // The shape in use: how many neighbouring pairs increase.
    let src = "fn main() -> Int {\n  [1, 3, 2, 5].windows(2).count(|p| p[1] > p[0])\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 2);
}

/// **ADR-149.** The only thing either grouping refuses. A run of zero elements
/// is not a short run — chunking a non-empty sequence into them has no finite
/// answer — and a negative one names nothing, so both raise `InvalidSize`
/// before they walk anything.
///
/// It is the fault `Vec(0 - 1, 0)` raises, reached from an *argument to a
/// pipeline row* for the first time (ADR-041 decision 1, ADR-146 decision 5).
#[test]
fn a_group_size_of_zero_or_less_faults() {
    for src in [
        "fn main() -> Int { [1, 2, 3].chunks(0).count() }",
        "fn main() -> Int { [1, 2, 3].windows(0).count() }",
        "fn main() -> Int { [1, 2, 3].chunks(0 - 1).count() }",
        "fn main() -> Int { [1, 2, 3].windows(0 - 1).count() }",
        // An empty receiver does not excuse the size: the question is still
        // unanswerable, and answering `[]` would hide a bug in the size.
        "fn main() -> Int {\n  var v = Vec()\n  v.chunks(0).count()\n}",
    ] {
        let (rt, _r) = run_main(src);
        assert!(rt.has_pending_fault(), "{src} must fault");
        assert_eq!(rt.fault(), praxis_runtime::FaultKind::InvalidSize, "{src}");
    }
}

/// **ADR-149, and ADR-127 decision 3 at two new rows.** A grouping is a
/// barrier: it takes any of the ten iterables, materialized first, and a chain
/// starts again from its `Vec[Vec[T]]` result.
///
/// The composition on *both* sides is the part worth pinning. A stage in front
/// of it feeds it (`filter(...).chunks(2)`), and a stage behind it consumes the
/// groups (`windows(2).map(|w| w.sum())`) — which only works if the result is a
/// real sequence of real sequences rather than a flattened one.
#[test]
fn a_grouping_takes_every_iterable_and_a_chain_starts_again_from_it() {
    // A `Range`, walked in place.
    let src = "fn main() -> Int {\n  var c = (0..6).chunks(3)\n  \
               c.count() * 10 + c[1][2]\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 25);

    // A `Text`, whose item is a `Char`.
    let src = "fn main() -> Char {\n  \"abcd\".windows(2)[1][1]\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_char(), 'c');

    // A `Set`, through its own snapshot accessor rather than `praxis_vec_get`.
    let src = "fn main() -> Int {\n  var s = Set()\n  s.insert(4)\n  \
               s.chunks(1).count() * 10 + s.chunks(1)[0][0]\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 14);

    // A stage in front: the groups are of what *survived*, not of the source.
    let src = "fn main() -> Int {\n  var c = [1, 2, 3, 4, 5].filter(|n| n % 2 == 1).chunks(2)\n  \
               c.count() * 100 + c[0][1] * 10 + c[1][0]\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 235);

    // …and a stage behind it, which needs each group to be a sequence in its
    // own right. `[1,2,3,4].windows(2)` summed is `[3, 5, 7]`, total 15.
    let src = "fn main() -> Int {\n  [1, 2, 3, 4].windows(2).map(|w| w.sum()).sum()\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 15);
}

/// **ADR-149.** A grouping reads no descriptor callback, so a `Vec` of closures
/// groups where `sorted()` and `unique()` are refused at check time — the same
/// claim `reversed` makes.
///
/// The other half is that a group is a *view*: the overlapping element of two
/// windows is one object, which is the language's reference semantics rather
/// than a rule of these rows. A mutation through one window is visible in the
/// next, and that is the same aliasing `var b = a` has.
#[test]
fn a_grouping_needs_nothing_of_its_elements_and_shares_them() {
    let src = "fn main() -> Int {\n  var v = Vec()\n  v.push(|x| x + 1)\n  \
               v.push(|x| x + 2)\n  v.push(|x| x + 3)\n  \
               v.windows(2).count() * 10 + v.chunks(2).count()\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 22);

    // The shared element: pushing through the first window's tail is seen
    // through the second window's head, because they are one `Vec`.
    let src = "fn main() -> Int {\n  var a = Vec()\n  var b = Vec()\n  var c = Vec()\n  \
               var outer = Vec()\n  outer.push(a)\n  outer.push(b)\n  outer.push(c)\n  \
               var w = outer.windows(2)\n  w[0][1].push(9)\n  w[1][0].count()\n}";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_int(), 1, "the overlapping element is one object");
}

// --- ADR-146: sized collection constructors ---------------------------------

/// `Vec(n, fill)` end to end: `n` slots, every one the fill, and zero is a
/// `Vec` rather than a refusal.
#[test]
fn a_sized_vec_runs() {
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var v = Vec(3, 7)\n  var t = 0\n  for x in v { t = t + x }\n  t * 10 + v.count()\n}",
    );
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 213, "three sevens, then the count");

    let (rt, result) = run_main("fn main() -> Int { Vec(0, 1).count() }");
    assert!(!rt.has_pending_fault());
    assert_eq!(
        result.as_int(),
        0,
        "an empty sized Vec is empty, not a fault"
    );

    // The element type comes from the fill with nothing written down, and the
    // slots are writable afterwards like any other `Vec`.
    let (rt, result) = run_main(
        "fn main() -> Bool {\n  var v = Vec(3, false)\n  v[1] = true\n  !v[0] && v[1] && !v[2]\n}",
    );
    assert!(!rt.has_pending_fault());
    assert!(result.as_bool());
}

/// `Grid(w, h, fill)` end to end: its extents, its cells, its bounds check and a
/// store that touches one cell and not its neighbour.
#[test]
fn a_sized_grid_runs() {
    let (rt, result) =
        run_main("fn main() -> Int {\n  var g = Grid(3, 2, 0)\n  g.width() * 10 + g.height()\n}");
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 32);

    let (rt, result) = run_main(
        "fn main() -> Int {\n  var g = Grid(3, 2, 0)\n  g[1, 1] = 9\n  g[0, 0] * 10 + g[1, 1]\n}",
    );
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 9, "one cell moved and the rest did not");

    // The bounds behaviour a hand-rolled `y * w + x` board does not have.
    let (rt, result) = run_main(
        "fn main() -> Bool {\n  var g = Grid(3, 2, '.')\n  g.contains(2, 1) && !g.contains(3, 1) && !g.contains(0 - 1, 0)\n}",
    );
    assert!(!rt.has_pending_fault());
    assert!(result.as_bool());

    // `Grid(0, 0, fill)` is `Grid()`: the empty grid is still reachable both
    // ways, which is ADR-146 decision 1's "the arity chooses the shape" seen
    // from the degenerate end.
    let (rt, result) = run_main("fn main() -> Int { Grid(0, 0, 1).cells().count() }");
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 0);
}

/// **ADR-146 decision 5.** A size the runtime cannot serve is a fault the
/// program can see, not an abort (ADR-041).
#[test]
fn a_negative_or_absurd_size_is_an_invalid_size_fault() {
    for src in [
        "fn main() -> Int { Vec(0 - 1, 0).count() }",
        "fn main() -> Int { Grid(0 - 1, 2, 0).width() }",
        "fn main() -> Int { Grid(2, 0 - 1, 0).width() }",
        // Multiplies cleanly and is still an allocation no host can serve.
        "fn main() -> Int { Grid(100000, 100000, 0).width() }",
        "fn main() -> Int { Vec(1000000000, 0).count() }",
    ] {
        let (rt, _r) = run_main(src);
        assert!(rt.has_pending_fault(), "{src} must fault");
        assert_eq!(rt.fault(), praxis_runtime::FaultKind::InvalidSize, "{src}");
    }
}

/// **ADR-146 decision 4**, pinned so that a later change to deep-copy the fill
/// is a failing test rather than a silent change of meaning. Every cell is the
/// *same* object, which is the aliasing `var b = a` and a double `push` of one
/// value already have.
#[test]
fn a_collection_fill_is_one_object_shared() {
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var g = Grid(2, 2, Vec())\n  g[0, 0].push(1)\n  g[1, 1].count()\n}",
    );
    assert!(!rt.has_pending_fault());
    assert_eq!(
        result.as_int(),
        1,
        "the four cells are one `Vec` (ADR-146 decision 4)"
    );

    // …and the same for `Vec(n, fill)`. This is also the case
    // `praxis_grid_new` refuses outright: `default_cell` has no zero value for
    // a composite, so an explicit fill is what makes a collection of
    // collections constructible at all.
    let (rt, result) = run_main(
        "fn main() -> Int {\n  var v = Vec(3, Vec())\n  v[0].push(1)\n  v[0].push(2)\n  v[2].count()\n}",
    );
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 2);
}

/// The fill is an arbitrary expression, and an expression that **allocates** is
/// the one the rooting has to survive: liveness names the fill as an operand of
/// the allocation, so `spill_roots` spills it before the wrapper that may
/// collect. Without that this is a use-after-free the pacer hides until the
/// heap is busy, which is what the loop is for.
#[test]
fn a_fill_that_allocates_survives_the_allocation_that_uses_it() {
    let (rt, result) = run_main(
        "fn main() -> Int {\n\
         \x20 var total = 0\n\
         \x20 var i = 0\n\
         \x20 while i < 5000 {\n\
         \x20   var v = Vec(20, \"a\" + \"b\")\n\
         \x20   total = total + v[19].len()\n\
         \x20   i = i + 1\n\
         \x20 }\n\
         \x20 total\n\
         }",
    );
    assert!(!rt.has_pending_fault());
    assert_eq!(result.as_int(), 10000, "5000 iterations of a 2-char fill");
}

// --- string interpolation (§8.1, ADR-147) ---------------------------------

/// **The gate for ADR-147 decision 2**, and the reason `praxis_value_to_text`
/// calls `GcRef::format` rather than rendering anything itself.
///
/// Each row is asserted against what `out` writes — obtained by *calling*
/// `GcRef::format`, the function `praxis_write_stdout` calls — and never against
/// a literal string. A test written against literals passes while the two
/// renderers agree by coincidence and keeps passing when one of them changes;
/// this one cannot pass while they disagree, which is the property ADR-143
/// decision 2 bought for the three scalar rows and ADR-147 extends to every
/// type.
///
/// The types here deliberately include several with **no** `to_text()` row and
/// no prospect of one, so this also pins that interpolation is not the
/// desugar-through-`to_text()` route ADR-143 decision 5 proposed.
///
/// `()` is absent from the list only because `fn main() { () }` currently
/// compiles clean and then has no entry point to run — a pre-existing gap
/// between `check` and `run` that has nothing to do with interpolation. The
/// `Unit` hole itself is covered by
/// `praxis_hir::infer_tests::a_hole_accepts_any_type`.
#[test]
fn a_hole_renders_what_out_renders() {
    for expr in [
        "[1, 2, 3]",
        "[\"a\", \"b\"]",
        "(1, \"x\")",
        "1.5",
        "'#'",
        "42",
        "true",
        "\"plain\"",
        "0..3",
        "Set[Int]()",
        "Map[Text, Int]()",
    ] {
        let hole_src = format!("fn main() -> Text {{\n  \"{{{expr}}}\"\n}}\n");
        let (rt, hole) = run_main(&hole_src);
        assert!(!rt.has_pending_fault(), "{expr} faulted: {:?}", rt.fault());

        // `out`'s own path: build the value and render it through `GcRef::format`.
        // Bound to a `var` first so every row takes one shape — a bare `()` as a
        // function body is a degenerate case this test is not about.
        let value_src = format!("fn main() {{\n  var probe = {expr}\n  probe\n}}\n");
        let (rt2, value) = run_main(&value_src);
        assert!(
            !rt2.has_pending_fault(),
            "{expr} faulted: {:?}",
            rt2.fault()
        );
        let mut want = String::new();
        value.format(&mut want);

        assert_eq!(
            hole.as_text(),
            want,
            "`\"{{{expr}}}\"` must render exactly what `out({expr})` writes"
        );
    }
}

/// The literal text around the holes survives, in order, with the escapes it
/// was written with — and an adjacent pair of holes has nothing between them.
#[test]
fn the_text_around_the_holes_is_kept_in_order() {
    for (src, want) in [
        ("\"Part 2: {40 + 2}\"", "Part 2: 42"),
        ("\"{1}{2}{3}\"", "123"),
        ("\"{1} and {2}\"", "1 and 2"),
        ("\"a{1}\"", "a1"),
        ("\"{1}b\"", "1b"),
        ("\"\\{{1}\\}\"", "{1}"),
        ("\"tab\\there: {1}\"", "tab\there: 1"),
    ] {
        let program = format!("fn main() -> Text {{\n  {src}\n}}\n");
        let (rt, result) = run_main(&program);
        assert!(!rt.has_pending_fault(), "{src} faulted: {:?}", rt.fault());
        assert_eq!(result.as_text(), want, "{src}");
    }
}

/// A hole holds a full expression, and it is evaluated where it is written —
/// left to right, so a hole with an effect runs in source order.
#[test]
fn a_hole_evaluates_a_full_expression_in_source_order() {
    let src = "\
fn bump(v: Vec[Int], n: Int) -> Int {
  v.push(n)
  n
}
fn main() -> Text {
  var seen = Vec[Int]()
  var line = \"{bump(seen, 1)}-{bump(seen, 2)}-{bump(seen, 3)}\"
  \"{line} {seen}\"
}
";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_text(), "1-2-3 [1, 2, 3]");
}

/// **The end-to-end half of ADR-147 decision 1.** A closure body that names an
/// outer binding *only inside a hole* still captures it.
///
/// The failure this guards is not a diagnostic: an implementation that re-lexed
/// holes later would allocate the closure with an empty environment and read a
/// slot nothing filled, so the program runs and answers something else. That is
/// why the assertion is on the value and not on a compile result.
#[test]
fn a_closure_captures_the_name_in_its_hole() {
    let src = "\
fn main() -> Text {
  var outer = 41
  var f = |n: Int| \"outer + n = {outer + n}\"
  f(1)
}
";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_text(), "outer + n = 42");
}

/// **ADR-147's consequence, measured.** An `out(label); out(value)` pair is one
/// call for *every* type, not just the four with a `to_text()` row — which is
/// the difference between this and `a_labelled_line_is_one_call`.
#[test]
fn a_labelled_line_is_one_call_for_any_type() {
    let src = "fn main() -> Text {\n  var splits = [1, 2, 3]\n  \"splits: {splits}\"\n}\n";
    let (rt, result) = run_main(src);
    assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
    assert_eq!(result.as_text(), "splits: [1, 2, 3]");
}
