//! Integration tests for the `praxis run` command — the Milestone 4 acceptance
//! criteria (§19): execute boxed integer arithmetic, branches, loops, and
//! recursive function calls; faults return to the host without unwinding.
//!
//! These drive the compiled `praxis` binary end to end (parse → analyze → typed
//! HIR → MIR → Cranelift JIT → execute), asserting on stdout and the exit code.

use std::path::PathBuf;
use std::process::Command;

fn bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_praxis"))
}

fn fixture(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures/run");
    p.push(name);
    p
}

/// Run a fixture and return (exit_code, stdout, stderr). Panics if the process
/// can't be spawned.
fn run_fixture(name: &str) -> (i32, String, String) {
    let output = Command::new(bin_path())
        .arg("run")
        .arg(fixture(name))
        .output()
        .expect("failed to run praxis");
    let code = output.status.code().unwrap_or(-1);
    (
        code,
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Assert a run-pass fixture exits 0 and prints exactly `expected` on stdout.
fn assert_passes(name: &str, expected: &str) {
    let (code, stdout, stderr) = run_fixture(name);
    assert_eq!(
        code, 0,
        "`{name}` should exit 0\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert_eq!(
        stdout.trim(),
        expected,
        "`{name}` should print {expected:?}, got {stdout:?}"
    );
}

/// Assert a run-fault fixture exits 1 and mentions `fault_msg` on stderr (and
/// crucially was NOT killed by a signal — no Rust panic/abort across the ABI).
fn assert_faults(name: &str, fault_msg: &str) {
    let (code, _stdout, stderr) = run_fixture(name);
    assert_eq!(
        code, 1,
        "`{name}` should exit 1 (fault), got code {code}\nstderr: {stderr}"
    );
    assert!(
        stderr.contains(fault_msg),
        "`{name}` should report `{fault_msg}`, got stderr: {stderr}"
    );
}

#[test]
fn run_pass_constant() {
    assert_passes("constant.px", "42");
}

#[test]
fn run_pass_arithmetic() {
    // 1 + 2 * 3 = 7 (precedence respected).
    assert_passes("arithmetic.px", "7");
}

#[test]
fn run_pass_branch() {
    assert_passes("branch.px", "100");
}

#[test]
fn run_pass_loop_sum() {
    // Sum 1..=5 = 15.
    assert_passes("loop_sum.px", "15");
}

#[test]
fn run_pass_recursive_factorial() {
    assert_passes("factorial.px", "120");
}

#[test]
fn run_pass_recursive_fibonacci() {
    assert_passes("fibonacci.px", "55");
}

#[test]
fn run_fault_overflow() {
    // §19 acceptance: overflow returns to the host without Rust unwinding.
    assert_faults("overflow.px", "integer overflow");
}

#[test]
fn run_fault_division_by_zero() {
    // §19 acceptance: division by zero returns to the host without unwinding.
    assert_faults("div_by_zero.px", "division by zero");
}

#[test]
fn run_fault_does_not_abort() {
    // The fault must surface as exit code 1, not as a signal (which would be
    // the case if a Rust panic crossed the ABI and aborted the process).
    let (code, _, _) = run_fixture("overflow.px");
    assert_ne!(
        code, -1,
        "process was killed by a signal (abort/panic leaked across the ABI)"
    );
}

// ===========================================================================
// M10 WS4 — §9.6 noninteractive crash diagnostic.
//
// A runtime fault now renders the fault line + a numbered backtrace + the
// top frame's locals (via praxis-debugger), instead of a bare one-liner. These
// tests assert the §9.6 output is present on stderr and the exit code is 1.
// The `--debug=never` flag forces the noninteractive path regardless of TTY.
// ===========================================================================

/// Run a fixture with explicit `--debug` mode, returning (exit, stdout, stderr).
fn run_fixture_debug(name: &str, debug: &str) -> (i32, String, String) {
    let output = Command::new(bin_path())
        .args(["run", "--debug", debug])
        .arg(fixture(name))
        .output()
        .expect("failed to run praxis");
    let code = output.status.code().unwrap_or(-1);
    (
        code,
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn m10ws4_noninteractive_renders_backtrace_and_locals() {
    // The §9.6 output: fault line, a numbered Backtrace section, and the top
    // frame's locals — including the named `xs` Vec with its elements.
    let (code, _stdout, stderr) = run_fixture_debug("debug_backtrace.px", "never");
    assert_eq!(code, 1, "fault exits 1");
    assert!(stderr.contains("program faulted: index out of bounds"));
    assert!(stderr.contains("Backtrace:"), "backtrace header present");
    assert!(stderr.contains("#0"), "backtrace numbers frames");
    assert!(stderr.contains("main"), "frame name shown");
    // The named local `xs` with its Vec value renders.
    assert!(
        stderr.contains("xs = [11, 22]"),
        "named local renders with value: {stderr}"
    );
}

#[test]
fn m10ws4_debug_never_exits_one_without_repl_note() {
    // `--debug=never` must not print the "REPL not wired" note — it declines
    // the REPL outright.
    let (code, _stdout, stderr) = run_fixture_debug("overflow.px", "never");
    assert_eq!(code, 1);
    assert!(!stderr.contains("interactive crash REPL"));
    assert!(stderr.contains("integer overflow"));
}

#[test]
fn m10ws4_default_auto_non_tty_is_noninteractive() {
    // In a test (no TTY), the default `auto` mode behaves like `never`: it
    // prints the noninteractive diagnostic and exits 1.
    let (code, _stdout, stderr) = run_fixture("overflow.px");
    assert_eq!(code, 1);
    assert!(
        stderr.contains("Backtrace:"),
        "auto/non-TTY still renders backtrace"
    );
}
