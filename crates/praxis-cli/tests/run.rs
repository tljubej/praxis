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
