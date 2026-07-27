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
fn m10ws4_debug_never_exits_one_without_repl() {
    // `--debug=never` must not enter the REPL — it prints the noninteractive
    // diagnostic and exits. No "Praxis crash>" prompt on stderr.
    let (code, _stdout, stderr) = run_fixture_debug("overflow.px", "never");
    assert_eq!(code, 1);
    assert!(!stderr.contains("Praxis crash>"));
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

// ===========================================================================
// M10 WS5 — interactive crash REPL (§9.4).
//
// `--debug=always` enters the REPL after a fault. Program input comes from
// `--input` (freeing stdin for REPL commands). These tests pipe a command
// script and assert the REPL's output (backtrace, frame navigation, locals).
// ===========================================================================

/// Run a fixture with `--debug=always`, piping `repl_cmds` to stdin and using
/// `--input` (empty) so stdin is free for the REPL. Returns (exit, combined).
fn run_repl_with_cmds(name: &str, repl_cmds: &str) -> (i32, String) {
    use std::process::Stdio;
    let mut child = Command::new(bin_path())
        .args(["run", "--debug=always", "--input", "/dev/null"])
        .arg(fixture(name))
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn praxis");
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().expect("stdin");
        stdin
            .write_all(repl_cmds.as_bytes())
            .expect("write repl cmds");
    }
    let output = child.wait_with_output().expect("wait");
    let code = output.status.code().unwrap_or(-1);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (code, combined)
}

#[test]
fn m10ws5_repl_bt_and_locals_and_quit() {
    // Pipe `bt`, `locals`, `quit` into the REPL. The output must contain the
    // backtrace frame, the named local, and exit cleanly (code 1).
    let (code, out) = run_repl_with_cmds("debug_backtrace.px", "bt\nlocals\nquit\n");
    assert_eq!(code, 1, "faulted run exits 1 after REPL quits");
    assert!(out.contains("Praxis crash>"), "REPL prompt shown: {out}");
    assert!(out.contains("#0"), "bt ran: {out}");
    assert!(out.contains("main"), "frame name shown: {out}");
    // The named local `xs = [11, 22]` renders in `locals`.
    assert!(out.contains("xs = [11, 22]"), "locals ran: {out}");
}

#[test]
fn m10ws5_repl_frame_navigation() {
    // `frame 0` selects the (only) frame; `up` at the outermost reports the
    // boundary. The selection is reflected in subsequent output.
    let (_code, out) = run_repl_with_cmds("debug_backtrace.px", "frame 0\nup\nquit\n");
    assert!(out.contains("frame 0:"), "frame select ran: {out}");
    assert!(out.contains("outermost"), "up-at-top boundary: {out}");
}

#[test]
fn m10ws5_repl_eof_exits() {
    // EOF (no `quit`) must exit the REPL cleanly, not hang.
    let (code, _out) = run_repl_with_cmds("debug_backtrace.px", "");
    assert_eq!(code, 1, "EOF exits the REPL with the fault exit code");
}

#[test]
fn m10ws5_repl_help_lists_commands() {
    let (_code, out) = run_repl_with_cmds("debug_backtrace.px", "help\nquit\n");
    for cmd in ["bt", "frame", "up", "down", "locals", "quit"] {
        assert!(out.contains(cmd), "help lists `{cmd}`: {out}");
    }
}

// ===========================================================================
// M10b WS3 — `source` / `input` / `parser` context commands (§9.4).
//
// `source` renders the selected frame's source extent (threaded in WS1) from
// the session's source text. `input`/`parser` render the §7.11 ParseDetail.
// ===========================================================================

#[test]
fn m10b_ws3_source_renders_faulting_function_text() {
    // `source` on the faulting `main` frame prints the function's source lines
    // (the whole `fn main … { … }` extent, threaded in WS1) with a caret.
    let (_code, out) = run_repl_with_cmds("debug_backtrace.px", "source\nquit\n");
    assert!(
        out.contains("main:"),
        "source shows the frame header: {out}"
    );
    assert!(
        out.contains("xs.get(99)"),
        "source shows the faulting line: {out}"
    );
    assert!(out.contains('^'), "source shows a caret: {out}");
}

#[test]
fn m10b_ws3_source_help_lists_command() {
    let (_code, out) = run_repl_with_cmds("debug_backtrace.px", "help\nquit\n");
    assert!(out.contains("source"), "help lists `source`: {out}");
    assert!(out.contains("input"), "help lists `input`: {out}");
    assert!(out.contains("parser"), "help lists `parser`: {out}");
}
