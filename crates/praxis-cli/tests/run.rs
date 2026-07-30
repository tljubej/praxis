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
fn missing_explicit_input_file_is_a_usage_error() {
    let missing = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/run/definitely-missing-input.txt");
    let output = Command::new(bin_path())
        .args(["run", "--input"])
        .arg(&missing)
        .arg(fixture("constant.px"))
        .output()
        .expect("failed to run praxis");
    let code = output.status.code().unwrap_or(-1);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(code, 2, "unreadable explicit input is a usage error");
    assert!(
        stderr.contains("failed to read") && stderr.contains("input"),
        "the input I/O error must be reported, got: {stderr}"
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

// ---- Float (§4.12) ----

#[test]
fn run_pass_float_literal() {
    assert_passes("float_literal.px", "2.5");
}

#[test]
fn run_pass_float_arith() {
    // 1.5 + 2.5 * 2.0 = 6.5.
    assert_passes("float_arith.px", "6.5");
}

#[test]
fn run_pass_float_methods() {
    // sqrt(16) + 5.to_float() = 4 + 5 = 9.
    assert_passes("float_methods.px", "9");
}

#[test]
fn run_pass_float_div_by_zero() {
    // 1.0 / 0.0 = inf (IEEE-754); Float division never faults (§4.12).
    assert_passes("float_div_by_zero.px", "inf");
}

#[test]
fn run_fault_float_to_int_nan() {
    // NaN → to_int faults with FloatToInt (§4.12), exit 1, no abort.
    assert_faults(
        "float_to_int_nan.px",
        "float-to-int conversion out of range",
    );
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

// ===========================================================================
// `out(...)` and `Unit`-returning `main` (§16.1, §4.3).
//
// `out` is `(T) -> Unit`: it writes its argument once and returns `Unit`. A
// `Unit`-returning `main` has no answer value, so the host prints nothing for
// it (no trailing result line) — the program's output is whatever `out` wrote.
// These guard against a former double-print bug where `out` returned its
// argument and the host re-printed `main`'s result.
// ===========================================================================

#[test]
fn unit_main_out_prints_argument_once() {
    // `out("kurac")` must write "kurac" exactly once; no second line from the
    // host printing `main`'s (Unit) result.
    assert_passes("unit_main_out.px", "kurac");
}

#[test]
fn unit_main_empty_prints_nothing() {
    // A `Unit`-returning `main` with no `out` produces empty stdout — not a
    // spurious "0" or "Unit" result line.
    let (code, stdout, stderr) = run_fixture("unit_main_empty.px");
    assert_eq!(code, 0, "should exit 0\nstdout: {stdout}\nstderr: {stderr}");
    assert_eq!(
        stdout, "",
        "empty Unit main should print nothing, got {stdout:?}"
    );
}

#[test]
fn no_return_type_main_defaults_to_unit() {
    // `fn main()` with no declared return type defaults to `Unit`, so `out`
    // writes once and nothing else is printed.
    assert_passes("no_return_type_main.px", "hi");
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
    // The named local `xs` with its Vec value renders (now in the `locals:`
    // section with a type column: `xs: <type> = [11, 22]`).
    assert!(
        stderr.contains("xs:") && stderr.contains("[11, 22]"),
        "named local renders with value: {stderr}"
    );
    // Temps are now in a separate `temps:` section, annotated with the
    // expression they materialized (`@ "..."`).
    assert!(stderr.contains("temps:"), "temps section present: {stderr}");
    assert!(
        stderr.contains("xs.get(99)"),
        "faulting temp shows its materializing expression: {stderr}"
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
    // The named local `xs: <type> = [11, 22]` renders in the `locals:` section.
    assert!(
        out.contains("xs:") && out.contains("[11, 22]"),
        "locals ran: {out}"
    );
    assert!(out.contains("locals:"), "locals section header: {out}");
    assert!(out.contains("temps:"), "temps section header: {out}");
}

#[test]
fn m11_locals_split_users_and_temps_with_types() {
    // A program with three `let` locals and a binop chain that overflows. The
    // `locals` display must: (1) separate user bindings into a `locals:` section
    // with a type column, (2) list compiler temps in a `temps:` section with a
    // per-frame id and type, and (3) keep the user's variables visible rather
    // than buried among temps.
    let (code, out) = run_repl_with_cmds("debug_temps.px", "locals\nquit\n");
    assert_eq!(code, 1, "overflow faults and exits 1 after REPL quits");
    // User locals render as `name: Type = value`.
    assert!(out.contains("a: Int = 10"), "user local a with type: {out}");
    assert!(out.contains("b: Int = 20"), "user local b with type: {out}");
    assert!(out.contains("c: Int = 30"), "user local c with type: {out}");
    // Temps render as `<tmp#N: Type>` in their own section.
    assert!(out.contains("temps:"), "temps section header: {out}");
    assert!(
        out.contains("<tmp#") && out.contains(": Int>"),
        "temps tagged with id and type: {out}"
    );
    // Literal and binop materialization temps show their provenance too — not
    // just call results — so an opaque `<tmp>` is never left unexplained.
    assert!(
        out.contains("@ \"10\""),
        "literal temp shows its source: {out}"
    );
    assert!(
        out.contains("@ \"a + b\""),
        "binop temp shows its source: {out}"
    );
    assert!(
        out.contains("@ \"a + b + c + 9223372036854775807\""),
        "faulting binop temp shows its source: {out}"
    );
}

#[test]
fn m11_temp_provenance_shows_materializing_expression() {
    // The faulting method-call temp must show the expression it materialized
    // (`@ "xs.get(99)"`), so the user can tell *what* a temp is rather than
    // staring at an opaque `<tmp>`.
    let (code, out) = run_repl_with_cmds("debug_backtrace.px", "locals\nquit\n");
    assert_eq!(code, 1);
    assert!(
        out.contains("@ \"xs.get(99)\""),
        "faulting temp shows its materializing expression: {out}"
    );
    assert!(
        out.contains("@ \"xs.push(11)\""),
        "push temp shows its expression too: {out}"
    );
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

// ===========================================================================
// M10b WS4 — `p EXPR` / `type EXPR` read-only JIT evaluator (§9.5).
//
// The fixture `debug_backtrace.px` has `xs = [11, 22]` in the faulting `main`
// frame. `p EXPR` synthesizes `fn __p_expr(xs: Vec[Int]) { EXPR }`, type-checks
// against the snapshot local, purity-gates, JITs, and calls with the snapshot's
// `xs` GcRef. `type EXPR` reports the inferred type without JIT.
// ===========================================================================

#[test]
fn m10b_ws4_p_literal_arithmetic() {
    // `p 1 + 2` → 3. No locals needed; a pure literal expression.
    let (_code, out) = run_repl_with_cmds("debug_backtrace.px", "p 1 + 2\nquit\n");
    assert!(out.contains("3"), "p 1 + 2 should print 3: {out}");
}

#[test]
fn m10b_ws4_p_evaluates_pure_method_on_snapshot_local() {
    // `p xs.len()` → the Vec's length, 2. A pure method call on a snapshot local.
    let (_code, out) = run_repl_with_cmds("debug_backtrace.px", "p xs.len()\nquit\n");
    assert!(out.contains("2"), "p xs.len() should print 2: {out}");
}

#[test]
fn m10b_ws4_p_index_into_snapshot_vec() {
    // `p xs.get(0)` → 11. Indexes the snapshot Vec via a pure method. This is
    // the case that needs the full static `Vec[Int]` type (WS1) to type-check.
    let (_code, out) = run_repl_with_cmds("debug_backtrace.px", "p xs.get(0)\nquit\n");
    assert!(out.contains("11"), "p xs.get(0) should print 11: {out}");
}

#[test]
fn m10b_ws4_p_rejects_mutation() {
    // `p xs.push(99)` is impure → the purity gate rejects it.
    let (_code, out) = run_repl_with_cmds("debug_backtrace.px", "p xs.push(99)\nquit\n");
    assert!(
        out.contains("error") && out.contains("impure"),
        "p xs.push(99) should be rejected as impure: {out}"
    );
}

#[test]
fn m10b_ws4_type_reports_collection_type() {
    // `type xs` → Vec[Int]. Proves the full static type (WS1) renders.
    let (_code, out) = run_repl_with_cmds("debug_backtrace.px", "type xs\nquit\n");
    assert!(
        out.contains("Vec[Int]"),
        "type xs should be Vec[Int]: {out}"
    );
}

#[test]
fn m10b_ws4_type_reports_inferred_method_type() {
    // `type xs.len()` → Int.
    let (_code, out) = run_repl_with_cmds("debug_backtrace.px", "type xs.len()\nquit\n");
    assert!(out.contains("Int"), "type xs.len() should be Int: {out}");
}

// ===========================================================================
// M10b WS5 — `heap EXPR` recursive inspection (§9.4).
//
// `heap EXPR` evaluates the expression (reusing the WS4 evaluator + purity
// gate) and renders the result prefixed with its type, so the structure and
// type are visible at a glance.
// ===========================================================================

#[test]
fn m10b_ws5_heap_shows_value_with_type() {
    // `heap xs` → `Vec[Int]: [11, 22]`. The type prefix distinguishes `heap`
    // from `p` (which prints just `[11, 22]`).
    let (_code, out) = run_repl_with_cmds("debug_backtrace.px", "heap xs\nquit\n");
    assert!(
        out.contains("Vec[Int]") && out.contains("[11, 22]"),
        "heap xs should show type + value: {out}"
    );
}

#[test]
fn m10b_ws5_heap_literal() {
    // `heap 1 + 2` → `Int: 3`.
    let (_code, out) = run_repl_with_cmds("debug_backtrace.px", "heap 1 + 2\nquit\n");
    assert!(
        out.contains("Int"),
        "heap 1 + 2 should show Int type: {out}"
    );
    assert!(out.contains("3"), "heap 1 + 2 should show value 3: {out}");
}

// ===========================================================================
// M10b WS6 — `restart` / `reload` (§9.7).
//
// `restart` reruns the same compiled code+input (re-faulting deterministically).
// `reload` re-reads the source from disk, recompiles, and reruns — discarding
// old JIT/snapshots only after the new compile succeeds. A failed recompile
// leaves the session intact with the old snapshot (the §9.7 guarantee).
// ===========================================================================

#[test]
fn m10b_ws6_restart_refaults_deterministically() {
    // `restart` re-runs the same faulting program. The re-run must fault again
    // (same kind) and produce a fresh snapshot the REPL can inspect.
    let (_code, out) = run_repl_with_cmds("debug_backtrace.px", "restart\nbt\nquit\n");
    assert!(
        out.contains("program faulted"),
        "restart should re-fault: {out}"
    );
    // The re-run's snapshot is inspectable: `bt` after restart lists frames.
    // (The output has two `#0 main` lines — one from the original banner, one
    // from the post-restart `bt`.)
    assert!(
        out.matches("#0").count() >= 2,
        "bt after restart runs against the new snapshot: {out}"
    );
}

#[test]
fn m10b_ws6_reload_after_edit_changes_result() {
    // Write a faulting fixture to a temp file, then `reload` after rewriting it
    // to a clean program. The reload re-reads the source, recompiles, and the
    // re-run completes (no fault).
    use std::io::{Read, Write};
    let dir = std::env::temp_dir();
    let src_path = dir.join("m10b_ws6_reload.px");
    {
        let mut f = std::fs::File::create(&src_path).unwrap();
        f.write_all(b"fn main() -> Int { 1 / 0 }").unwrap();
    }
    // Start the REPL against the faulting version.
    use std::process::Stdio;
    let mut child = Command::new(bin_path())
        .args(["run", "--debug=always", "--input", "/dev/null"])
        .arg(&src_path)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut stdin = child.stdin.take().expect("stdin");
    // Wait for the child to fault and print the REPL prompt before rewriting
    // the source (otherwise the child reads the edited file at startup). Poll
    // stderr until the prompt appears.
    let stderr = child.stderr.as_mut().expect("stderr");
    let mut seen = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        let mut buf = [0u8; 256];
        match stderr.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                seen.extend_from_slice(&buf[..n]);
                if String::from_utf8_lossy(&seen).contains("Praxis crash>") {
                    break;
                }
            }
        }
    }
    assert!(
        String::from_utf8_lossy(&seen).contains("Praxis crash>"),
        "REPL should start before reload: {}",
        String::from_utf8_lossy(&seen)
    );
    // Now safe to rewrite: the child has read the original faulting source.
    {
        let mut f = std::fs::File::create(&src_path).unwrap();
        f.write_all(b"fn main() -> Int { 42 }").unwrap();
    }
    stdin.write_all(b"reload\nquit\n").unwrap();
    drop(stdin);
    let output = child.wait_with_output().expect("wait");
    let combined = format!(
        "{}{}{}",
        String::from_utf8_lossy(&seen),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = std::fs::remove_file(&src_path);
    assert!(
        combined.contains("program completed"),
        "reload after edit should run cleanly: {combined}"
    );
    assert!(
        combined.contains("42"),
        "reload should reflect the edited source: {combined}"
    );
}

#[test]
fn m10b_ws6_reload_on_malformed_source_keeps_session() {
    // §9.7: a failed recompilation leaves the crash REPL active with the old
    // snapshot. Write a valid faulting fixture, start the REPL, then `reload`
    // after rewriting it to malformed source. The reload must error and the old
    // snapshot stays inspectable.
    use std::io::{Read, Write};
    let dir = std::env::temp_dir();
    let src_path = dir.join("m10b_ws6_reload_bad.px");
    {
        let mut f = std::fs::File::create(&src_path).unwrap();
        f.write_all(b"fn main() -> Int { 1 / 0 }").unwrap();
    }
    use std::process::Stdio;
    let mut child = Command::new(bin_path())
        .args(["run", "--debug=always", "--input", "/dev/null"])
        .arg(&src_path)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut stdin = child.stdin.take().expect("stdin");
    let stderr = child.stderr.as_mut().expect("stderr");
    let mut seen = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        let mut buf = [0u8; 256];
        match stderr.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                seen.extend_from_slice(&buf[..n]);
                if String::from_utf8_lossy(&seen).contains("Praxis crash>") {
                    break;
                }
            }
        }
    }
    assert!(
        String::from_utf8_lossy(&seen).contains("Praxis crash>"),
        "REPL should start: {}",
        String::from_utf8_lossy(&seen)
    );
    // Rewrite to malformed source (unbalanced).
    {
        let mut f = std::fs::File::create(&src_path).unwrap();
        f.write_all(b"fn main() -> Int {").unwrap();
    }
    stdin.write_all(b"reload\nbt\nquit\n").unwrap();
    drop(stdin);
    let output = child.wait_with_output().expect("wait");
    let combined = format!(
        "{}{}{}",
        String::from_utf8_lossy(&seen),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = std::fs::remove_file(&src_path);
    assert!(
        combined.contains("error") && combined.contains("unchanged"),
        "reload on malformed source should error and keep the session: {combined}"
    );
    // The old snapshot is still inspectable: `bt` runs.
    assert!(
        combined.contains("#0"),
        "bt runs against the old snapshot after a failed reload: {combined}"
    );
}

// ===========================================================================
// REP-19: a top-level statement executes (ADR-067)
// ===========================================================================

/// **REP-19's headline.** A file's top-level statements are its program, and
/// §3.2 has said so since the design doc was written: "top-level statements are
/// wrapped in a generated entry function."
///
/// Nothing wrapped them. `TypedModule.items` held only `fn` declarations,
/// `lower_module` emitted only those, and the host called `main` — so
/// `out(1)\nlet x = 2\nout(x)` passed `praxis check` and printed **nothing**,
/// then exited 1 with "no `main` function to run". §3.3 and §4.2 are written
/// entirely at top level, so the design doc's own programs are what that
/// silenced.
#[test]
fn a_top_level_statement_runs_in_the_order_it_is_written() {
    // Every top-level statement kind: a call, a `let` read by a later one, a
    // `var`, and an assignment to it. Order is the assertion — a set of
    // statements that ran in the wrong order prints the same three lines.
    assert_passes("top_level_statements.px", "1\n2\n3");

    // Declarations interleave with statements and do not move them: `double` is
    // declared between two `out`s and callable by both the one after it and the
    // one after that.
    assert_passes("top_level_calls_a_declared_fn.px", "1\n4\n6");
}

/// `fn main` is an ordinary function when the file has top-level statements, and
/// the entry point when it does not (ADR-067).
///
/// The fallback is what keeps every program written in the `fn main` convention
/// working — the whole corpus, and every end-to-end test in this file. The design
/// doc never mentions a `main`.
#[test]
fn a_declared_main_is_the_entry_point_only_when_nothing_else_is() {
    // No top-level statements: `main` runs, exactly as before — both a
    // `Unit`-returning one, whose output is its `out(…)` calls, and an
    // `Int`-returning one, whose answer the host prints.
    assert_passes("unit_main_out.px", "kurac");
    assert_passes("constant.px", "42");

    // Both: the top level runs, and `main` runs because the top level calls it.
    // Once — a rule that ran the top level *and then* called `main` would print
    // `2` twice.
    assert_passes("top_level_beside_fn_main.px", "1\n2\n3");

    // Neither: the file declares a function and never calls it, so there is
    // nothing to run. The message names both spellings, because either one would
    // have made it a program.
    let (code, stdout, stderr) = run_fixture("no_statements_and_no_main.px");
    assert_eq!(code, 1, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stderr.contains("no statements to run") && stderr.contains("`main`"),
        "the error names both ways to have an entry point: {stderr}"
    );
}

/// The entry point's own name is `<entry>`, which is **not an identifier** — so
/// no program can declare a second function with it, and no program can call it.
///
/// That is ADR-064's rule for the subscript rows applied to the one other name
/// the compiler mints into the same namespace. The crash debugger renders it, so
/// a fault in a top-level statement names a frame the user can recognize as not
/// theirs.
#[test]
fn the_entry_points_name_is_not_one_a_program_can_spell() {
    let dir = std::env::temp_dir().join("praxis_rep19_entry_name");
    std::fs::create_dir_all(&dir).unwrap();
    let src_path = dir.join("entry.px");

    // A fault in a top-level statement reaches the crash debugger, and the frame
    // it names is the generated one.
    std::fs::write(&src_path, "let v = Vec()\nout(v.get(0))\n").unwrap();
    let output = Command::new(bin_path())
        .arg("run")
        .arg(&src_path)
        .output()
        .expect("failed to run praxis");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("index out of bounds"),
        "a top-level statement's fault reaches the host: {combined}"
    );
    assert!(
        combined.contains("<entry>"),
        "the generated frame is named, and named unspellably: {combined}"
    );

    // `<entry>` is not a name the parser can produce, so a program cannot
    // declare one — the closest spelling is a parse error rather than a second
    // definition.
    std::fs::write(&src_path, "fn <entry>() { out(1) }\n").unwrap();
    let output = Command::new(bin_path())
        .arg("check")
        .arg(&src_path)
        .output()
        .expect("failed to run praxis");
    assert_ne!(
        output.status.code().unwrap_or(-1),
        0,
        "`fn <entry>()` must not be a declaration"
    );

    let _ = std::fs::remove_file(&src_path);
}

/// RT-13, at the surface. An enum value renders its **variant name**: the
/// runtime carries an `EnumSchema` now, so `Number(7)` prints as `Number(7)`
/// rather than as the `<variant 2: 7>` a value whose whole identity was its tag
/// could only manage.
///
/// `Some`/`None` are here beside a declared enum on purpose — the prelude
/// `Option` is one enum def like any other (F12), and a `Some` the program
/// wrote must render the same way as one the runtime built.
#[test]
fn run_pass_enum_renders_its_variant_name() {
    assert_passes(
        "enum_variant_names.px",
        "Empty\nWall\nNumber(7)\nSome(3)\nNone\nSome(x)",
    );
}
