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
        stderr.contains("unexpected byte"),
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

#[test]
fn unimplemented_commands_exit_two() {
    // The stubbed commands must not pretend to succeed.
    for cmd in ["repl", "lsp"] {
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
