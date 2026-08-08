//! Integration test for the `praxis check` command: a `.px` file is loaded and
//! diagnosed through the CLI.

use std::path::PathBuf;
use std::process::Command;

mod common;

use common::{bin_path, fixture};

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
        stderr.contains("var first = @"),
        "missing source line: {stderr}"
    );
    assert!(stderr.contains("^"), "missing caret: {stderr}");
    assert!(stderr.contains("2 error(s)"), "missing summary: {stderr}");
}

#[test]
fn parse_error_file_reports_multiple_diagnostics() {
    // The parser produces multiple diagnostics from one malformed file and the
    // CLI surfaces them end to end.
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
    // Cross-type `var` reassignment is rejected end to end through
    // `praxis check`, surfacing a Y001 type diagnostic.
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
/// `lsp` is not in this list: the server is real, and `tests/lsp.rs` drives a
/// scripted JSON-RPC session against the same binary. `watch` is not here
/// because it takes a file argument and is covered where the run tests live.
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
/// There is no interior: the token has no closing backtick, so handing the
/// capture-body scanner the whole token would fabricate a nested template the
/// file does not contain, at an offset where nothing is.
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
/// An unbounded token would run to EOF: `T002`'s caret would cover the rest of
/// the file, the `}` closing the enclosing block would be swallowed inside it
/// and produce a `P001` for a block that never closed, and a `Y001` would
/// follow that — one typo, three errors, two of them about damage the first
/// caused.
///
/// **The count is the assertion**, not "it reports `T002`": what this pins is
/// that nothing else is reported and that the caret is one line.
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

/// `--help` is user-facing text, and it names nothing only the implementers can
/// read.
///
/// A clap doc comment *is* the help page, so a spec reference or a milestone
/// marker written there reaches the user, who has no idea what `§7.1` is and no
/// way to find out from there. Such notes belong in a plain `//` comment beside
/// the doc comment, where clap cannot reach them.
///
/// Every help page, not just the root: flags carry these markers too, and clap
/// prints those under `run --help`.
#[test]
fn no_help_page_leaks_an_implementation_marker() {
    for args in [
        vec!["--help"],
        vec!["run", "--help"],
        vec!["check", "--help"],
        vec!["watch", "--help"],
        vec!["repl", "--help"],
        vec!["lsp", "--help"],
    ] {
        let out = Command::new(bin_path())
            .args(&args)
            .output()
            .expect("failed to run praxis");
        let help = String::from_utf8_lossy(&out.stdout).to_string()
            + &String::from_utf8_lossy(&out.stderr);
        for marker in ["§", "Milestone", "(M1", "M6)", "M10)", "M11)"] {
            assert!(
                !help.contains(marker),
                "`praxis {}` prints `{marker}`:\n{help}",
                args.join(" ")
            );
        }
    }
}

/// …and the stub commands do not name a milestone either.
///
/// A number in "planned for Milestone N" goes stale the moment the milestone
/// passes, and neither `watch` nor `repl` has a scheduled one, so the honest
/// message names no number at all.
#[test]
fn a_stub_command_does_not_name_a_milestone() {
    for args in [vec!["repl"], vec!["watch", "prog.px"]] {
        let out = Command::new(bin_path())
            .args(&args)
            .output()
            .expect("failed to run praxis");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{stderr}");
        assert!(stderr.contains("not implemented"), "{stderr}");
        assert!(
            !stderr.contains("Milestone"),
            "`praxis {}` names a milestone: {stderr}",
            args.join(" ")
        );
    }
}

/// A scratch directory this **process** owns, for the tests below that write a
/// source file for the binary to read. `run.rs`'s helper, for its reason: two
/// concurrent `cargo test` processes share `/tmp`, and a fixed name in it means
/// one rewrites a file the other is checking.
fn scratch_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("check-tests-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create this process's scratch directory");
    dir
}

/// `praxis check` over `src`, returning (exit code, stderr).
fn check_source(name: &str, src: &str) -> (i32, String) {
    let path = scratch_dir().join(name);
    std::fs::write(&path, src).expect("write the source");
    let out = Command::new(bin_path())
        .arg("check")
        .arg(&path)
        .arg("--color")
        .arg("never")
        .output()
        .expect("failed to run praxis");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// **Gate 3 at the CLI**, which is where a user meets it (ADR-141).
///
/// `"##"[0]` is a well-typed program that quietly means `#` and `""[0]` is an
/// index fault at run time. Both mistakes become lexical, with a code and a
/// caret over the literal — and `'##'` carries the rewrite the author meant.
#[test]
fn a_char_literal_that_is_not_one_character_is_reported_at_the_literal() {
    let (code, stderr) = check_source("char_len.px", "var two = '##'\nvar none = ''\n");
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("error[T007]"), "{stderr}");
    assert!(
        stderr.contains("a character literal holds exactly one character"),
        "{stderr}"
    );
    assert!(
        stderr.contains("empty character literal"),
        "the two messages are distinct: {stderr}"
    );
    assert!(
        stderr.contains("^^^^"),
        "the caret covers the whole literal: {stderr}"
    );
    // The machine-applicable fix (ADR-132).
    assert!(stderr.contains("\"##\""), "{stderr}");
    assert!(stderr.contains("2 error(s)"), "{stderr}");
}

/// **The readability half, and the one a user actually notices.** An
/// unterminated character literal reports once — a lexer that treated `'` as an
/// unknown character instead would turn one typo into a cascade of `P001`,
/// `P002` and `N001` reports about the text between the quotes.
#[test]
fn an_unterminated_char_literal_does_not_cascade() {
    let (code, stderr) = check_source("char_unterminated.px", "var c = 'a\nout(c)\n");
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("error[T006]"), "{stderr}");
    assert!(
        stderr.contains("unterminated character literal"),
        "{stderr}"
    );
    assert!(stderr.contains("1 error(s)"), "exactly one: {stderr}");
    for cascade in ["P001", "P002", "T003", "N001"] {
        assert!(
            !stderr.contains(cascade),
            "a `{cascade}` cascade is back: {stderr}"
        );
    }
}

/// The escape set is a text literal's plus `\'`, and a `\u{…}` is refused here
/// exactly as it is inside `"…"` — one language, one escape table.
#[test]
fn a_char_literals_escapes_are_a_text_literals() {
    let (code, stderr) = check_source(
        "char_escapes.px",
        "var a = '\\n'\nvar b = '\\''\nvar c = '\\\\'\nout(a)\nout(b)\nout(c)\n",
    );
    assert_eq!(code, 0, "{stderr}");

    let (code, stderr) = check_source("char_bad_escape.px", "var a = '\\u{41}'\n");
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("error[T005]"), "{stderr}");
    assert!(
        stderr.contains("invalid escape in character literal"),
        "{stderr}"
    );
}
