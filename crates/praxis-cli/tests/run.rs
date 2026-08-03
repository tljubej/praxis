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

/// A scratch directory this **process** owns, for the tests that must write a
/// file for the binary to read.
///
/// Five tests wrote fixed names straight into `std::env::temp_dir()`
/// (`praxis-rep60-empty.in`, `m10b_ws6_reload.px`, …). Within one run that is
/// fine, and one helper even says so — "named after the calling test so two
/// tests cannot race for it". Between runs it is not: **two concurrent
/// `cargo test` processes share `/tmp` and clobber each other's fixtures**, so
/// one rewrites a source file while the other's REPL is reading it. That is not
/// hypothetical — it was measured while two agents ran the suite at once, and
/// four different tests in this file failed across four runs while every one of
/// them passed in isolation. A test that fails only when something else is
/// running is worse than a failing test: it teaches you to re-run instead of to
/// look.
///
/// `CARGO_TARGET_TMPDIR` alone does not fix it — it is the same path for both
/// processes. The pid is what makes it exclusive.
fn scratch_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("run-tests-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create this process's scratch directory");
    dir
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

// ---- Lazy standard input (§7.10, REP-51) ----

/// Run a fixture with stdin bound to a pipe this test **keeps open**, writing
/// `stdin` into it but never sending EOF until the deadline. Returns the exit
/// code and stdout, or `None` if the child was still running after `deadline`.
///
/// A never-closed pipe is the shape a terminal and a CI harness both have, and
/// it is the only shape that can tell an eager read from a lazy one: against
/// `/dev/null` — which `Command::output` uses by default, and which is why no
/// existing test here noticed — an eager read returns immediately.
///
/// The deadline is what keeps a regression a *failure*: without it, restoring
/// the eager read would wedge this test process rather than fail it.
fn run_with_open_stdin(
    name: &str,
    stdin: &str,
    deadline: std::time::Duration,
) -> Option<(i32, String)> {
    use std::io::Write;
    let mut child = Command::new(bin_path())
        .arg("run")
        .arg(fixture(name))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn praxis");
    // Written, then *held*: the pipe stays open, so a read to EOF blocks.
    let mut pipe = child.stdin.take().expect("piped stdin");
    if !stdin.is_empty() {
        pipe.write_all(stdin.as_bytes()).expect("write to child");
        pipe.flush().expect("flush to child");
    }

    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => break Some(status),
            None if start.elapsed() >= deadline => break None,
            None => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    };
    match status {
        None => {
            let _ = child.kill();
            let _ = child.wait();
            None
        }
        Some(status) => {
            drop(pipe);
            let out = child.wait_with_output().expect("wait_with_output");
            Some((
                status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stdout).into_owned(),
            ))
        }
    }
}

/// **REP-51's gate.** A program with no `read` never touches standard input.
///
/// §7.10: "The first `read` lazily reads standard input once into an immutable
/// GC-managed source buffer." The host read it to EOF *before* calling the
/// entry function instead, so a `read`-free program consumed stdin anyway — and
/// against a pipe nobody closes, which is what a terminal and a CI harness both
/// are, `praxis run` blocked forever waiting for an EOF that was not coming.
/// Every `praxis run` of a `read`-free program from a terminal hung.
///
/// The open pipe is the whole test. Every other test in this file goes through
/// `Command::output`, which binds stdin to `/dev/null`, where an eager read
/// returns instantly — which is exactly why the defect survived a suite this
/// size. Without the fix this call returns `None` at the deadline.
#[test]
fn a_program_that_never_reads_does_not_wait_for_standard_input() {
    let deadline = std::time::Duration::from_secs(10);
    let outcome = run_with_open_stdin("constant.px", "", deadline);
    let (code, stdout) = outcome.expect(
        "`praxis run` blocked on standard input for a program with no `read` \
         (§7.10: the *first* `read` reads it)",
    );
    assert_eq!(code, 0, "stdout: {stdout}");
    assert_eq!(stdout.trim(), "42");
}

/// The other half, and it is a **mutation companion, not a gate**: it passes on
/// `main`, where the read is eager. Laziness must not become "never" — the
/// cheapest way to stop the hang is to stop reading standard input at all, and
/// that would pass the gate above.
///
/// So: the *same* open pipe, a program that does `read`, and the child must
/// still be running at the deadline. A `read` reads to EOF (§7.10 — the buffer
/// is the whole input, not a stream), and this test deliberately withholds the
/// EOF, so "still waiting" is the correct behaviour and "exited" would mean the
/// input was never read.
#[test]
fn a_program_that_reads_still_waits_for_its_input() {
    let outcome = run_with_open_stdin(
        "reads_lines_of_int.px",
        "1\n2\n3\n",
        std::time::Duration::from_secs(2),
    );
    assert!(
        outcome.is_none(),
        "a `read` reads to EOF; this pipe has sent none, so the program \
         cannot have finished — laziness must not mean the input is skipped, \
         got {outcome:?}"
    );
}

/// Run a fixture with `stdin` piped and closed, and assert stdout.
fn assert_passes_with_stdin(name: &str, stdin: &str, expected: &str) {
    use std::io::Write;
    let mut child = Command::new(bin_path())
        .arg("run")
        .arg(fixture(name))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn praxis");
    {
        let mut pipe = child.stdin.take().expect("piped stdin");
        pipe.write_all(stdin.as_bytes()).expect("write to child");
    }
    let out = child.wait_with_output().expect("wait_with_output");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert_eq!(stdout.trim(), expected);
}

/// Reading twice reuses the buffer rather than consuming a stream (§7.10), and
/// the lazy read is what installs it.
#[test]
fn a_second_read_sees_the_same_buffer() {
    assert_passes_with_stdin("reads_lines_of_int.px", "1\n2\n3\n", "3\n3");
}

/// **ADR-114.** A parse whose input forces the native root store past its
/// reservation still answers, end to end.
///
/// `parser::walk_lines` opens **one** `NativeScope` for the whole `lines(…)`
/// walk and roots one reference per input line, so the store's high-water mark
/// is the input's line count — 200,001 for a 200,000-line file, measured. That
/// is what makes the store a growable one rather than a second ADR-101, and the
/// growth is where a pointer-shaped watermark would have died: the array moves,
/// and every scope that saved its position before the move publishes a stale
/// address on the way out.
///
/// The unit tests in `praxis-runtime::roots` pin the mechanism at every corner.
/// This one exists because they force the growth *synthetically*, and this is
/// the shape a real program reaches it through: 4096 lines is four doublings
/// past `NATIVE_ROOT_RESERVATION`, through the interpreter, with the collector
/// pacing underneath.
#[test]
fn a_parse_that_outgrows_the_native_root_reservation_still_answers() {
    let lines: String = (0..4096).map(|n| format!("{n}\n")).collect();
    assert_passes_with_stdin("reads_lines_of_int.px", &lines, "4096\n4096");
}

/// **REP-60.** A zero-byte `--input` file is *empty input*, not the absence of
/// input.
///
/// The buffer was installed only `if !t.is_empty()`, so an empty file left
/// `ctx.input_source` at the immortal Unit. `Input::new` answers `None` for a
/// non-Text source and takes the "no detail was recorded" path, so every `read`
/// faulted with `program faulted: input parse mismatch` and **nothing else** —
/// no offset, no `expected`, no `actual`, and no mention of the file being
/// empty. §7.11 asks a parse detail to name where parsing broke; the one shape
/// that could not say anything at all was the one the message was least able to
/// be guessed from.
///
/// `lines(int)` over a zero-length buffer is `[]` by `split_lines`'s own rule,
/// so the program answers rather than faults. That is also what makes the gate
/// red before the fix without asserting on message text: it exits 1 today.
#[test]
fn a_zero_byte_input_file_is_empty_input_and_not_a_contentless_fault() {
    let empty = scratch_dir().join("praxis-rep60-empty.in");
    std::fs::write(&empty, b"").expect("write the empty input file");
    let output = Command::new(bin_path())
        .args(["run", "--debug=never", "--input"])
        .arg(&empty)
        .arg(fixture("reads_lines_of_int.px"))
        .output()
        .expect("failed to run praxis");
    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let _ = std::fs::remove_file(&empty);

    assert_eq!(
        code, 0,
        "`read lines(int)` over an empty file is the empty list, not a fault\n\
         stdout: {stdout}\nstderr: {stderr}"
    );
    // The fixture reads twice and prints each length.
    assert_eq!(stdout.trim(), "0\n0", "stderr: {stderr}");
}

/// Run a fixture under `--debug=never` with `stdin` piped and then **closed**,
/// returning `(exit code, stdout, stderr)`.
///
/// The closed empty pipe is the shape that matters here and it is not the same
/// shape as `run_with_open_stdin`'s: this one sends EOF, so a `read` completes
/// with whatever arrived — for `stdin = ""`, zero bytes. It is also not
/// `Command::output`'s default, which binds stdin to `/dev/null`; both reach the
/// reader with an empty answer, and a test that means "the user piped nothing"
/// should say so rather than lean on a default.
fn run_with_closed_stdin(name: &str, stdin: &str) -> (i32, String, String) {
    use std::io::Write;
    let mut child = Command::new(bin_path())
        .args(["run", "--debug=never"])
        .arg(fixture(name))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn praxis");
    {
        let mut pipe = child.stdin.take().expect("piped stdin");
        pipe.write_all(stdin.as_bytes()).expect("write to child");
    }
    let out = child.wait_with_output().expect("wait_with_output");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Run a fixture under `--debug=never` with `--input` pointed at a file holding
/// `contents`, returning `(exit code, stdout, stderr)`. The file is named after
/// the calling test so two tests cannot race for it.
fn run_with_input_file(name: &str, contents: &str, tag: &str) -> (i32, String, String) {
    let path = scratch_dir().join(format!("praxis-{tag}.in"));
    std::fs::write(&path, contents).expect("write the input file");
    let output = Command::new(bin_path())
        .args(["run", "--debug=never", "--input"])
        .arg(&path)
        .arg(fixture(name))
        .output()
        .expect("failed to run praxis");
    let _ = std::fs::remove_file(&path);
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// **REP-60's stdin half, and the twin of the `--input` gate above.** A reader
/// that answers zero bytes has given *empty input*, not no input.
///
/// `praxis_get_input` installed the buffer only `if !bytes.is_empty()`, so
/// standard input with nothing in it left `ctx.input_source` at the immortal
/// Unit and `praxis_run_parser`'s §6.3 descriptor guard faulted before the
/// parser ran — `program faulted: input parse mismatch` and nothing else. The
/// `--input` half of REP-60 had already dropped the same guard, so the two
/// spellings of "run this with no input" gave different answers: `--input` on an
/// empty file printed `0\n0` and exited 0 while `< /dev/null` exited 1 with a
/// contentless fault. ADR-087 is the record; the rule lives at
/// `praxis_get_input`.
///
/// **Observed red** with step 1 of the repair reverted (the `if
/// !bytes.is_empty()` wrapper restored in `praxis_get_input`): rc=1 with
/// `error: program faulted: input parse mismatch` on stderr instead of `0\n0` on
/// stdout.
#[test]
fn empty_standard_input_is_empty_input_and_not_a_contentless_fault() {
    assert_passes_with_stdin("reads_lines_of_int.px", "", "0\n0");
}

/// The field assertion, and the reason the row was worth opening: a program that
/// *requires* content still faults on empty input, and now the fault carries
/// §7.11's fields.
///
/// §7.11: "A mismatch creates a runtime fault containing: input span / parser
/// span / expected description / actual preview / parser path / partial root
/// value." A fault raised *before* any buffer existed can carry none of them —
/// it has no input span to name — which is why the contentless message was not
/// merely terse but unfixable in place. With a zero-length buffer installed, the
/// parse actually runs and fails where it should: at `0..0`, wanting an `int`.
///
/// This is deliberately not "the program succeeds": a repair that only made
/// `reads_lines_of_int.px` pass would leave this one contentless.
///
/// **Observed red** with step 1 reverted: rc was 1 (correctly), but stderr held
/// neither `at input offset 0..0` nor `expected int`.
#[test]
fn empty_standard_input_faults_with_an_offset_and_an_expectation() {
    let (code, stdout, stderr) = run_with_closed_stdin("reads_an_int.px", "");
    assert_eq!(
        code, 1,
        "`read int` over an empty buffer is a mismatch\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("at input offset 0..0"),
        "the mismatch must name where it happened (§7.11 input span): {stderr}"
    );
    assert!(
        stderr.contains("expected int"),
        "the mismatch must name what it wanted (§7.11 expected description): {stderr}"
    );
}

/// **A mutation companion, not a gate** — it is green today and stays green.
///
/// It pins the one §7.11 field the test above legitimately cannot assert: a
/// zero-length buffer has no bytes to preview, so `actual:` is correctly absent
/// from an empty-input mismatch. Assert it here, on a non-empty failing input,
/// so "no `actual` line" stays a property of the empty buffer rather than
/// becoming a property of the renderer.
#[test]
fn a_failing_read_names_what_it_saw() {
    let (code, stdout, stderr) = run_with_closed_stdin("reads_an_int.px", "x\n");
    assert_eq!(code, 1, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stderr.contains("at input offset 0..0") && stderr.contains("expected int"),
        "{stderr}"
    );
    assert!(
        stderr.contains("actual: x⏎"),
        "a non-empty failing input previews what was there (§7.11 actual preview): {stderr}"
    );
}

/// **The "one rule" gate.** The two spellings of "run this with no input" must
/// answer identically — not merely both plausibly.
///
/// Asserting the two behaviours separately would let them drift apart again the
/// next time one path is touched; asserting they are *equal* is what makes the
/// rule checkable. A user cannot be expected to know that `< /dev/null` and
/// `--input /dev/null` are different questions.
///
/// **Observed red** with step 1 reverted: both exited 1, but the stderrs
/// differed — the `--input` run carried `at input offset 0..0: expected int`
/// and the stdin run carried no detail lines at all.
#[test]
fn empty_stdin_and_a_zero_byte_input_file_answer_the_same() {
    let piped = run_with_closed_stdin("reads_an_int.px", "");
    let filed = run_with_input_file("reads_an_int.px", "", "rep60-same-answer");
    assert_eq!(
        piped, filed,
        "empty standard input and a zero-byte `--input` file are the same \
         question and must get the same answer (REP-60, ADR-087)"
    );
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
    // sqrt(16.0) + 5.to_float() = 4.0 + 5.0 = 9.0.
    //
    // **This assertion used to read `"9"`** (REP-44, §8.2). It was not wrong
    // about the arithmetic and it was not wrong about the type — it was wrong
    // that a `Float` may render as an `Int`. `9` is not a `Float` literal in
    // this language (§4.12: `42` is strictly an `Int`, and the two never mix),
    // so the text it asserted did not read back as the value it came from, and
    // the same characters are what a `Vec[Int]` of `[9]` would print. ADR-083
    // decided the rendering; the expected text moves with it.
    assert_passes("float_methods.px", "9.0");
}

#[test]
fn run_pass_float_div_by_zero() {
    // 1.0 / 0.0 = inf (IEEE-754); Float division never faults (§4.12).
    assert_passes("float_div_by_zero.px", "inf");
}

/// **REP-50's gate.** The `-0.0` literal, and the round trip ADR-083 states.
///
/// A Float negation was lowered as `0.0 - x`, and `0.0 - 0.0` is `+0.0`, so
/// `-0.0` evaluated to `+0.0`: `out(-0.0)` printed `0.0`, and the text a Float
/// rendered to did not read back as that Float — which is the one rule ADR-083
/// exists to state. ADR-045 had already decided the two zeros are distinct
/// values (§16.3 orders a container by the rendered form, and the two forms
/// differ), so the sign was a value the language admits and the evaluator lost.
///
/// The observation is `1.0 / x` and **not** `x == 0.0`: IEEE-754 says
/// `-0.0 == 0.0`, so equality is blind to precisely the bit this is about, and
/// a gate written with `==` would pass before the fix. Lines 1–2 (the computed
/// negative zero) were already right and are the companion; lines 3–6 are the
/// literal and the same negation through a binding, and both answered `0.0` /
/// `inf` before the fix.
#[test]
fn run_pass_float_negative_zero() {
    assert_passes(
        "float_negative_zero.px",
        "-0.0\n-inf\n-0.0\n-inf\n-0.0\n-inf\ninf\ninf\n-2.5\n-2.5",
    );
}

/// **REP-64's gate.** A compound assignment on a `Float` is Float arithmetic —
/// at every operator, through every target shape the language has.
///
/// A `Float` rides the uniform `i64` scalar channel as its IEEE-754 **bit
/// pattern** (ADR-037), so every arithmetic site has to bit-cast to `f64` and
/// back. Both compound-assignment paths in MIR — `x += …` on a binding and
/// `m[k] += …` through a subscript — forgot, and did integer arithmetic on the
/// pattern instead: `var f = 1.0; f += 2.0; out(f)` printed
/// `9218868437227405312`, which is `f64::to_bits(1.0) + f64::to_bits(2.0)`. The
/// plain binary `+` had never forgotten, so `f = f + 2.0` was right in the same
/// program.
///
/// Every operand is picked so the old path is a **silent wrong answer** with
/// `rc=0` rather than a crash — an integer overflow would have been caught by
/// any test that ran the program at all, and the fixture's comments carry the
/// pre-fix value line by line. `-0.0` is observed through `1.0 / x` and not
/// `x == 0.0`, because IEEE-754 says `-0.0 == 0.0` and equality is blind to
/// exactly the bit those two lines are about (REP-50).
#[test]
fn run_pass_float_compound_assign() {
    let expected = [
        // A binding: `+=`, `-=`, `/=`, then `*=` read through the sign of zero.
        "3.0", "3.0", "2.5", "-0.0", "-inf",
        // Operands whose bit patterns read as negative integers, then `-0.0`.
        "1.0", "1.0", "3.0", "0.0", "inf",
        // A binding captured by a closure, so the slot is a `VarCell`.
        "0.0", // A subscript store: `+=`, `-=`, `/=`, `*=` (via `1/x`), mixed signs.
        "3.0", "3.0", "2.5", "-inf", "1.0", // The neighbours this must not have changed.
        "3", "3", "ab", "3.0",
    ]
    .join("\n");
    assert_passes("float_compound_assign.px", &expected);
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

/// **RED ON PURPOSE, and the reason it is here at all.**
///
/// Handover 26 said four times that W8-S0 lands
/// `m11_locals_split_users_and_temps_with_types` red and called that its
/// measurement signal. It does not: every assertion in that test is a
/// *provenance string*, never a value. Handover 27 §1 read the chain — the
/// fixture makes `a + b` an interior node whose producer ADR-120 deletes; the
/// debug store is driven by `praxis_mir::defs`, so the slot is never written;
/// `render.rs` keeps an uninit temp that has a span; and the span survives
/// because `build_function_debug_meta` emits a `DebugLocalMeta` for every `Gc`
/// local, defined or not. So the temp silently degrades from `= 30` to
/// `= <uninit>` and nothing goes red.
///
/// This is the assertion that goes red, and it was added by ADR-120 part 1
/// *before* the pass landed so that part 2 (W8-S0b, the scalar debug slot) has
/// something to turn green **unedited**. Do not relax it, do not delete it, and
/// do not "fix" it by editing what it expects: the whole of its value is that a
/// §9 debugger guarantee cannot be narrowed without a test saying so.
///
/// `crates/praxis-codegen-cranelift/tests/jit.rs`'s
/// `a_temp_that_never_reached_a_shadow_slot_is_still_renderable` is the same
/// regression one layer down, at the crash-snapshot API rather than the
/// rendered text, and it is red for the same reason. Handover 27 §9 asked
/// whether such a test existed outside `run.rs`; it does, and finding it was
/// worth more than the guess.
#[test]
fn a_forwarded_binop_temp_still_renders_the_value_it_materialized() {
    let (code, out) = run_repl_with_cmds("debug_temps.px", "locals\nquit\n");
    assert_eq!(code, 1, "overflow faults and exits 1 after REPL quits");
    assert!(
        out.contains("@ \"a + b\" = 30"),
        "the `a + b` temp renders its value, not `<uninit>`: {out}"
    );
}

/// **All three of them**, and the third is the one nobody predicted.
///
/// ADR-120 decision 6 measured the part-1 regression as three temps of the
/// fixture's seven going `<uninit>`, not the one handover 27 §1 traced:
/// `@ "a + b"` 30, `@ "a + b + c"` 60, and `@ "9223372036854775807"` — an
/// out-of-range `Int` literal whose `Alloc{Int}` producer is in the forwarded
/// set. `@ "10"`, `@ "20"` and `@ "30"` never degraded, because a small-int
/// box is also `MoveGc`'d into the binding it initializes and that second
/// reader declines the forward.
///
/// The test above asserts the first. This asserts all three, so a part-2
/// implementation that repaired the `Materialize` case and missed the `Alloc`
/// one would be a failure rather than a partial success — and so the survivors
/// are pinned too: they are the control that says the fixture still contains
/// temps this transform does not touch.
#[test]
fn every_temp_the_forwarding_elided_renders_its_value_again() {
    let (code, out) = run_repl_with_cmds("debug_temps.px", "locals\nquit\n");
    assert_eq!(code, 1, "overflow faults and exits 1 after REPL quits");
    for expected in [
        "@ \"a + b\" = 30",
        "@ \"a + b + c\" = 60",
        "@ \"9223372036854775807\" = 9223372036854775807",
        // The three that never degraded, asserted so a change that "fixed" the
        // three above by writing every slot from somewhere else is still a
        // failure if it disturbed these.
        "@ \"10\" = 10",
        "@ \"20\" = 20",
        "@ \"30\" = 30",
    ] {
        assert!(out.contains(expected), "missing `{expected}`: {out}");
    }
    // And the faulting expression's own temp stays `<uninit>`: `render.rs`
    // keeps an uninit temp that has a span precisely so the user can see which
    // expression did not finish, and ADR-120 part 2 must not fill it in with
    // the wrapped sum the overflow produced on the way to the raise.
    assert!(
        out.contains("@ \"a + b + c + 9223372036854775807\" = <uninit>"),
        "the expression that faulted produced no value: {out}"
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

/// **REP-60's §9.7 half.** A `restart` against empty input must see the *same*
/// empty input — which means the same zero-length buffer, not no buffer.
///
/// `rerun_main` re-installed the session's input only `if
/// !self.input_text.is_empty()`, the same guard the `--input` path had already
/// shed. So the first fault carried `at input offset 0..0: expected int` and the
/// restarted one carried nothing, and `input` in the REPL answered `(no input
/// context — not a parse failure)` about a run that had failed to parse. §9.7
/// promises a restart is the same run; for zero-byte input it was not, and that
/// was true on the `--input` path that REP-60's first half was supposed to have
/// finished.
///
/// `--input /dev/null` is the zero-byte file (`run_repl_with_cmds` passes it).
///
/// **What this asserts, and why not the banner.** `restart`'s banner prints the
/// fault *kind* and the frame count and never the parse detail — for empty and
/// non-empty input alike (`Repl::do_restart_or_reload`). So "the detail line
/// appears twice" is not this row's property; it is not true of any input and
/// asserting it would fail for a reason that has nothing to do with REP-60. The
/// property that genuinely differed is what the REPL's `input` command can
/// answer *about the restarted run*, so that is what is asserted, together with
/// the empty/non-empty equivalence that is the whole point of ADR-087.
///
/// **Observed red** with step 4 of the repair reverted (the
/// `if !self.input_text.is_empty()` guard restored in
/// `DebugSession::rerun_main`): `input` after `restart` answered
/// `(no input context — not a parse failure)` — about a run that had failed to
/// parse — where the same drive over a non-empty failing file answered
/// `input at offset 0..0:`.
#[test]
fn a_restart_with_empty_input_sees_the_same_empty_input() {
    let (_code, out) = run_repl_with_cmds("reads_an_int.px", "restart\ninput\nquit\n");
    assert!(
        out.contains("input at offset 0..0:"),
        "after `restart`, the REPL's `input` must describe the same zero-length \
         buffer the first run parsed against (§9.7): {out}"
    );
    assert!(
        !out.contains("no input context"),
        "the restarted run *did* fail to parse, so `input` has a context to \
         report: {out}"
    );
}

#[test]
fn m10b_ws6_reload_after_edit_changes_result() {
    // Write a faulting fixture to a temp file, then `reload` after rewriting it
    // to a clean program. The reload re-reads the source, recompiles, and the
    // re-run completes (no fault).
    use std::io::{Read, Write};
    let dir = scratch_dir();
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
    let dir = scratch_dir();
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
    let dir = scratch_dir().join("praxis_rep19_entry_name");
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
