//! Integration test for the `praxis check` command — the Milestone 0
//! acceptance criterion: "a dummy `.px` file can be loaded and diagnosed
//! through the CLI".

use std::path::PathBuf;
use std::process::Command;

fn bin_path() -> PathBuf {
    // `CARGO_BIN_EXE_praxis` is set by cargo when running integration tests and
    // points at the compiled `praxis` binary.
    PathBuf::from(env!("CARGO_BIN_EXE_praxis"))
}

fn fixture(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures");
    p.push(name);
    p
}

#[test]
fn clean_file_exits_zero() {
    let output = Command::new(bin_path())
        .arg("check")
        .arg(fixture("clean.px"))
        .output()
        .expect("failed to run praxis");
    assert!(
        output.status.success(),
        "clean file should exit 0\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    // No error summary line on a clean check.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("error(s)"),
        "clean file should report no errors, got: {stderr}"
    );
}

#[test]
fn bad_byte_file_exits_nonzero_with_diagnostic() {
    let output = Command::new(bin_path())
        .arg("check")
        .arg(fixture("bad_byte.px"))
        .output()
        .expect("failed to run praxis");
    let code = output
        .status
        .code()
        .expect("process was terminated by signal");
    assert_eq!(code, 1, "file with a lex error should exit 1");

    // The rendered path is absolute (derived from CARGO_MANIFEST_DIR), so we
    // assert on the path-stable fragments of the diagnostic rather than
    // snapshotting the whole stderr.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("error[T003]"), "missing code: {stderr}");
    assert!(
        stderr.contains("unexpected character"),
        "missing message: {stderr}"
    );
    assert!(
        stderr.contains("bad_byte.px:4:"),
        "missing location: {stderr}"
    );
    assert!(
        stderr.contains("let first = @"),
        "missing source line: {stderr}"
    );
    assert!(stderr.contains("^"), "missing caret: {stderr}");
    assert!(stderr.contains("2 error(s)"), "missing summary: {stderr}");
}

#[test]
fn parse_error_file_reports_multiple_diagnostics() {
    // Milestone 1 acceptance: the parser produces multiple diagnostics from
    // one malformed file and the CLI surfaces them end to end.
    let output = Command::new(bin_path())
        .arg("check")
        .arg(fixture("parse_error.px"))
        .output()
        .expect("failed to run praxis");
    let code = output
        .status
        .code()
        .expect("process was terminated by signal");
    assert_eq!(code, 1, "file with parse errors should exit 1");

    let stderr = String::from_utf8_lossy(&output.stderr);
    // At least two distinct P0xx parse diagnostics.
    let p001_count = stderr.matches("error[P001]").count();
    assert!(
        p001_count >= 2,
        "expected >=2 parse diagnostics, got {p001_count}: {stderr}"
    );
}

#[test]
fn type_error_file_reports_y001() {
    // Milestone 2 acceptance: cross-type `var` reassignment is rejected end to
    // end through `praxis check`, surfacing a Y001 type diagnostic.
    let output = Command::new(bin_path())
        .arg("check")
        .arg(fixture("type_error.px"))
        .output()
        .expect("failed to run praxis");
    let code = output
        .status
        .code()
        .expect("process was terminated by signal");
    assert_eq!(code, 1, "file with a type error should exit 1");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("error[Y001]"),
        "missing Y001 code: {stderr}"
    );
    assert!(
        stderr.contains("expected Int, found Text"),
        "missing type-mismatch message: {stderr}"
    );
}

#[test]
fn missing_file_exits_two() {
    let output = Command::new(bin_path())
        .arg("check")
        .arg(fixture("does_not_exist.px"))
        .output()
        .expect("failed to run praxis");
    let code = output
        .status
        .code()
        .expect("process was terminated by signal");
    assert_eq!(code, 2, "missing file should exit 2 (usage error)");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("failed to read source file"),
        "missing file should explain itself, got: {stderr}"
    );
}

/// The stubbed commands must not pretend to succeed.
///
/// **`lsp` left this list at M11.** It was `["repl", "lsp"]` and asserting
/// `main.rs:82`'s `not_implemented("lsp", None, 11)`; the server is real now and
/// `crates/praxis-cli/tests/lsp.rs` drives a scripted JSON-RPC session against
/// the same binary instead. `watch` is not here because it takes a file
/// argument and is covered where the run tests live.
#[test]
fn unimplemented_commands_exit_two() {
    // One entry today. Kept as a list because `watch` joins it the moment it
    // takes an argument-free form, and because the shape says "these commands",
    // not "this command".
    #[allow(clippy::single_element_loop)]
    for cmd in ["repl"] {
        let output = Command::new(bin_path())
            .arg(cmd)
            .output()
            .expect("failed to run praxis");
        let code = output
            .status
            .code()
            .expect("process was terminated by signal");
        assert_eq!(code, 2, "`praxis {cmd}` should exit 2");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("not implemented"),
            "`praxis {cmd}` should report it is not implemented, got: {stderr}"
        );
    }
}

/// **An unterminated template reports once, and about itself.**
///
/// Under D10 a backtick inside a capture opens a template of its own, so a
/// capture with no closing brace — `` read `{int` `` — makes the run
/// non-terminating and the token runs to the end of the file. `T002` is the
/// truthful report of that.
///
/// What must not follow is a *second* diagnostic about the token's interior.
/// There is no interior: the token has no closing backtick. `convert_template`
/// used to fall back to `unwrap_or(&text)` and hand the scanner the whole token
/// including its opening backtick, which answered "malformed capture body at
/// byte 5: unterminated nested template" — a construct this file does not
/// contain, at an offset that is not where anything is. That is the IP-03 class
/// (a diagnostic describing something other than what was read), one layer up.
///
/// The scanner's own `unterminated_capture_errors` cannot see this: it calls
/// `scan_template` directly and never goes through the lexer. This test is the
/// user-facing path.
#[test]
fn an_unterminated_template_does_not_also_report_a_fabricated_interior() {
    let output = Command::new(bin_path())
        .arg("check")
        .arg(fixture("unterminated_template.px"))
        .output()
        .expect("failed to run praxis");
    assert_eq!(
        output.status.code().expect("terminated by signal"),
        1,
        "an unterminated template must fail the check"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("error[T002]") && stderr.contains("unterminated backtick template"),
        "the truthful report is missing: {stderr}"
    );
    assert!(
        !stderr.contains("nested template"),
        "reported a nested template the source does not contain: {stderr}"
    );
    assert!(
        !stderr.contains("error[I030]"),
        "reported about an interior the token does not have: {stderr}"
    );
}

/// **D18 / ADR-094.** An unterminated template names its own line, and is the
/// *only* thing reported.
///
/// The report used to be true and three times too wide. The token ran to EOF,
/// so `T002`'s caret covered the rest of the file; the `}` closing the enclosing
/// block was swallowed inside it, which produced a `P001` for the block that
/// never closed; and a `Y001` followed that. One typo, three errors, two of them
/// about damage the first one caused.
///
/// **The count is the assertion.** "It reports `T002`" was already true before
/// the fix and is the acceptance-not-value shape — what changed is that nothing
/// else is reported, and that the caret is one line.
///
/// Observed red before the rule landed: three errors, `T002` + `P001` + `Y001`.
/// Observed red at an intermediate state where the lexer bounded the token but
/// the parser did not accept the new token kind: three again, `T002` + `P001` +
/// `I000`.
#[test]
fn an_unterminated_template_names_its_own_line_and_nothing_else() {
    let output = Command::new(bin_path())
        .arg("check")
        .arg(fixture("unterminated_template.px"))
        .output()
        .expect("failed to run praxis");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stderr.contains("praxis: 1 error(s)"),
        "one typo is one error (ADR-094): {stderr}"
    );
    for cascade in ["error[P001]", "error[Y001]", "error[I000]", "error[Y023]"] {
        assert!(
            !stderr.contains(cascade),
            "{cascade} is damage the unterminated token used to cause: {stderr}"
        );
    }

    // The caret spans the template, not the rest of the file: an underline that
    // reached EOF would be far longer than the line that carries it.
    let caret = stderr
        .lines()
        .find(|l| l.contains('^'))
        .expect("the report draws a caret");
    let width = caret.chars().filter(|c| *c == '^').count();
    assert!(
        (1..=40).contains(&width),
        "the caret must cover the template, not the file: {width} carets in {caret:?}"
    );
}
