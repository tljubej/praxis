//! Every `.px` program in the workspace `tests/` tree runs (REP-12).
//!
//! The corpus under `tests/aoc-corpus` and `tests/input-parsers` is the design
//! doc's own acceptance material (§17.3, §19.8, §19.9) and **no Rust test
//! covered it**. That is how `day02_grid_of_char.px` sat broken: it calls
//! `map.len()` on a `Grid[Char]`, which is reported at *lowering*, so a
//! `praxis check` sweep was clean while `praxis run` exited 1. Reading a file is
//! not running it, and only the second one compiles.
//!
//! Each program is a triple:
//!
//! * `name.px`  — the program. Discovered by walking the tree, so a new one is
//!   covered the moment it lands.
//! * `name.out` — its expected stdout. **Required**; a program with no
//!   expectation fails the test rather than being skipped, because a skipped
//!   fixture is what this test exists to prevent.
//! * `name.in`  — its input, passed as `--input`. Optional: a program with no
//!   `read` needs none. A *missing* one for a program that reads is not silent
//!   either — the parse faults and the run exits 1.

use std::path::{Path, PathBuf};
use std::process::Command;

fn bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_praxis"))
}

/// The workspace `tests/` tree, from this crate's manifest directory.
fn corpus_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates/praxis-cli -> crates
    p.pop(); // crates -> workspace root
    p.push("tests");
    p
}

/// Every `.px` under `dir`, recursively, in a stable order.
fn programs(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
    let mut entries: Vec<_> = entries.map(|e| e.expect("dir entry").path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            found.extend(programs(&path));
        } else if path.extension().is_some_and(|e| e == "px") {
            found.push(path);
        }
    }
    found
}

#[test]
fn every_corpus_program_runs_and_prints_the_answer_it_documents() {
    let root = corpus_root();
    let programs = programs(&root);
    // A guard against the test silently covering nothing — a wrong `corpus_root`
    // would otherwise pass by finding no programs at all.
    //
    // **The floor is the actual count, not a comfortable margin below it.** At
    // `>= 13` the gate agreed with a handoff that said "25 programs (17
    // aoc-corpus, 8 input-parsers)" while the tree held 24 (18 and 6), and
    // neither number was checkable against the other. Bumping this when a
    // program lands is the price of the doc and the gate stating one fact.
    // Today: 34 under `tests/aoc-corpus`, 13 under `tests/input-parsers`.
    assert!(
        programs.len() >= 47,
        "expected at least 47 programs in the corpus under {}, found {}. \
         If you added one, raise this floor; if you removed one, say why.",
        root.display(),
        programs.len()
    );

    for px in programs {
        let expected = std::fs::read_to_string(px.with_extension("out")).unwrap_or_else(|e| {
            panic!(
                "{}: every corpus program needs a `.out` naming its expected \
                 stdout ({e}). Add one; do not skip the program.",
                px.display()
            )
        });

        let mut cmd = Command::new(bin_path());
        cmd.arg("run").arg(&px);
        let input = px.with_extension("in");
        if input.exists() {
            cmd.arg("--input").arg(&input);
        }
        // Never inherit the harness's stdin: a `read` program with no `.in`
        // must run against *empty* input rather than block on a terminal. It no
        // longer faults there — a zero-byte buffer is empty input, and the
        // constructors answer from their own rules (ADR-087) — so this line
        // buys determinism, not a fault. Every corpus program that reads has an
        // `.in` anyway.
        cmd.stdin(std::process::Stdio::null());
        let output = cmd.output().expect("failed to run praxis");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(0),
            "`{}` should run clean\nstdout: {stdout}\nstderr: {stderr}",
            px.display()
        );
        assert_eq!(
            stdout.trim(),
            expected.trim(),
            "`{}` printed the wrong answer\nstderr: {stderr}",
            px.display()
        );
    }
}
