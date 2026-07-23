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
    let ids = jit.compile(&funcs).expect("JIT compilation");
    (jit, ids)
}

/// Compile and call a zero-arg `fn main() -> Int { ... }`, returning the result
/// `GcRef` and the runtime (so the caller can read the fault / payload).
fn run_main(src: &str) -> (Runtime, GcRef) {
    let (jit, ids) = compile(src);
    let main_id = *ids.get("main").expect("no `main` function");
    let mut rt = Runtime::new();
    let mut ctx = rt.context();
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
