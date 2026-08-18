//! The design document's own programs, run through the real binary.
//!
//! # Why this file exists
//!
//! A fence is never retyped here. It is **extracted from
//! `docs/technical-design.md` at test time**, byte for byte, and driven through
//! the same two commands a reader would type. A test that quotes the doc can
//! drift from it; a test that reads it cannot. A paraphrase is a different
//! program — it can hold a call site the fence lacks, and then agree with the
//! compiler about a program the document does not contain.
//!
//! # What "clean" means for a fence
//!
//! §4.9's fence declares a function and nothing else, so `praxis run` has nothing
//! to execute and says so: `no statements to run and no `main` function`. That is
//! not a compile failure and it is not what this asserts about. What it asserts is
//! that **no language diagnostic** — no `P0xx`, `N0xx`, `Y0xx` or `Y1xx` — comes
//! out of either command. Those are the codes that mean the program is wrong.

use std::path::PathBuf;
use std::process::Command;

mod common;

use common::{bin_path, workspace_root};

/// The first ```praxis fence after the line whose text is `heading`, verbatim.
///
/// Panics rather than returning `None` if the heading or the fence is missing:
/// a gate that silently covers nothing is worse than no gate.
fn fence_after(doc: &str, heading: &str) -> String {
    let mut lines = doc.lines();
    lines
        .by_ref()
        .find(|l| l.trim() == heading)
        .unwrap_or_else(|| panic!("`{heading}` is not a heading in docs/technical-design.md"));
    let mut opened = false;
    let mut body = String::new();
    for line in lines.by_ref() {
        if !opened {
            if line.trim_end() == "```praxis" {
                opened = true;
            }
            continue;
        }
        if line.trim_end() == "```" {
            return body;
        }
        body.push_str(line);
        body.push('\n');
    }
    panic!("no closing ```praxis fence after `{heading}`");
}

/// Every `error[CODE]` line in `text`, in order.
fn codes(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| {
            let rest = l.strip_prefix("error[")?;
            let code = rest.split(']').next()?;
            Some(code.to_string())
        })
        .collect()
}

/// §4.9's fence, as the document writes it, compiles under both `praxis check`
/// and `praxis run`.
///
/// Nothing calls `manhattan`, so nothing ever says what `a` and `b` are: an
/// uncalled function whose parameters are only ever *read from* must not demand
/// a record definition no call site exists to supply.
#[test]
fn section_4_9s_function_example_checks_and_runs() {
    let root = workspace_root();
    let doc = std::fs::read_to_string(root.join("docs/technical-design.md"))
        .expect("docs/technical-design.md under the workspace root");
    let fence = fence_after(&doc, "### 4.9 Functions");
    // The fence this test is about, and a guard against extracting some other
    // one: `manhattan` is §4.9's opening example.
    assert!(
        fence.contains("fn manhattan(a, b)") && fence.contains("a.x"),
        "extracted the wrong fence from §4.9:\n{fence}"
    );

    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("design_doc_section_4_9.px");
    std::fs::write(&path, &fence).expect("write the extracted fence");

    for command in ["check", "run"] {
        let out = Command::new(bin_path())
            .arg(command)
            .arg(&path)
            .output()
            .expect("failed to run praxis");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            codes(&stderr).is_empty(),
            "`praxis {command}` on §4.9's own fence must report no diagnostic, got:\n{stderr}\
             \n--- the fence ---\n{fence}"
        );
    }
}

/// **ADR-090.** §7.7's "repeated labeled blocks" fence, as the document writes
/// it, runs against the input it describes.
///
/// `  Starting items: {items:csv(int)}` ends in its capture, so the capture is
/// its template's last part and `block` has to compute a window for it —
/// otherwise `csv` is handed the rest of the *section* and faults on the lines
/// that follow.
///
/// `tests/aoc-corpus/rep58_section_7_7_monkeys.px` carries the same fence and
/// asserts the values; this test carries the half a hand-copied fixture cannot —
/// it goes red if §7.7 is later edited into something that does not run.
///
/// **The exit code is the assertion, not `codes(&stderr)`.** A parse fault
/// prints `error: program faulted`, never an `error[CODE]` line, so a
/// codes-only assertion is green while the program faults.
#[test]
fn section_7_7s_repeated_labeled_blocks_example_runs() {
    let root = workspace_root();
    let doc = std::fs::read_to_string(root.join("docs/technical-design.md"))
        .expect("docs/technical-design.md under the workspace root");
    let fence = fence_after(&doc, "### 7.7 Repeated labeled blocks");
    // A guard against extracting some other fence.
    assert!(
        fence.contains("{items:csv(int)}") && fence.contains("Monkey {id:int}:"),
        "extracted the wrong fence from §7.7:\n{fence}"
    );

    let tmp = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let px = tmp.join("design_doc_section_7_7.px");
    std::fs::write(&px, &fence).expect("write the extracted fence");
    // The AoC-2022-day-11 sample the fence describes.
    let input = tmp.join("design_doc_section_7_7.in");
    std::fs::write(
        &input,
        "Monkey 0:\n  Starting items: 79, 98\n  Operation: new = old * 19\n  \
         Test: divisible by 23\n    If true: throw to monkey 2\n    \
         If false: throw to monkey 3\n\nMonkey 1:\n  Starting items: 54, 65, 75, 74\n  \
         Operation: new = old + 6\n  Test: divisible by 19\n    \
         If true: throw to monkey 2\n    If false: throw to monkey 0\n",
    )
    .expect("write the sample input");

    let out = Command::new(bin_path())
        .arg("check")
        .arg(&px)
        .output()
        .expect("failed to run praxis");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        codes(&stderr).is_empty(),
        "`praxis check` on §7.7's own fence must report no diagnostic, got:\n{stderr}\
         \n--- the fence ---\n{fence}"
    );

    let out = Command::new(bin_path())
        .arg("run")
        .arg(&px)
        .arg("--input")
        .arg(&input)
        .output()
        .expect("failed to run praxis");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "§7.7's own fence must run against the input it describes, got:\n{stderr}\
         \n--- the fence ---\n{fence}"
    );
}

/// Every ```praxis fence in the design document **parses**.
///
/// A labelled parser argument written bare is one way a fence fails this:
/// `chars(one_of("^v<>"), skip: whitespace)` outside `read` is an ordinary call
/// expression, and `skip:` is not a syntax the language has there. A labelled
/// argument *is* implemented — the parser emits `PARSER_NAMED_ARG` and
/// `parser_lower` consumes it — but only inside the **parser-expression
/// sublanguage**, which is entered by `read` or by `parse(text, …)` and nowhere
/// else (§7.1).
///
/// The sweep is the point. A per-fence assertion covers the fences someone
/// remembered; this covers the document, including fences added later.
///
/// **Parse only.** A fence is a fragment — most of them name a `struct` declared
/// three sections earlier, or a binding the surrounding prose supplies — so
/// `N0xx` and `Y0xx` are expected and are not what this asserts. `P0xx` is
/// different in kind: it means the text is not the language, whatever the
/// context around it would have been.
#[test]
fn every_praxis_fence_in_the_design_doc_parses() {
    let root = workspace_root();
    let doc = std::fs::read_to_string(root.join("docs/technical-design.md"))
        .expect("docs/technical-design.md under the workspace root");

    let tmp = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let mut fences = 0;
    let mut failures = Vec::new();
    let mut lines = doc.lines().enumerate();
    while let Some((i, line)) = lines.next() {
        if line.trim_end() != "```praxis" {
            continue;
        }
        let mut body = String::new();
        for (_, inner) in lines.by_ref() {
            if inner.trim_end() == "```" {
                break;
            }
            body.push_str(inner);
            body.push('\n');
        }
        fences += 1;
        let path = tmp.join(format!("design_doc_fence_{}.px", i + 1));
        std::fs::write(&path, &body).expect("write the extracted fence");
        let out = Command::new(bin_path())
            .arg("check")
            .arg(&path)
            .output()
            .expect("failed to run praxis");
        let stderr = String::from_utf8_lossy(&out.stderr);
        let parse_errors: Vec<_> = codes(&stderr)
            .into_iter()
            .filter(|c| c.starts_with('P'))
            .collect();
        if !parse_errors.is_empty() {
            failures.push(format!(
                "docs/technical-design.md:{}: {parse_errors:?}\n{body}",
                i + 1
            ));
        }
    }

    // A guard against the sweep covering nothing — a fence-detection bug would
    // otherwise pass by finding no fences at all.
    assert!(
        fences >= 57,
        "expected the design doc's ```praxis fences, found {fences}"
    );
    assert!(
        failures.is_empty(),
        "{} of {fences} fences do not parse:\n\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// **ADR-093.** Appendix D — the document's own "first end-to-end demo target" —
/// checks clean, runs, and prints the answers it is a demo of.
///
/// The guard below keeps it exercising the §6.3 barrier rows (`sorted`,
/// `frequencies`) alongside the pipeline methods, so a missing catalog row for
/// any of them is a `Y110` here. Both commands have to be silent, not just
/// `run`: a method that cannot resolve is reported at `check`, not left to
/// lowering.
#[test]
fn appendix_ds_demo_checks_runs_and_prints_its_answer() {
    let root = workspace_root();
    let doc = std::fs::read_to_string(root.join("docs/technical-design.md"))
        .expect("docs/technical-design.md under the workspace root");
    let fence = fence_after(&doc, "## Appendix D: First end-to-end demo target");
    // A guard against extracting some other fence, and against Appendix D being
    // edited into something that no longer exercises the barriers.
    assert!(
        fence.contains(".sorted()") && fence.contains(".frequencies()"),
        "extracted the wrong fence from Appendix D:\n{fence}"
    );

    let tmp = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let px = tmp.join("design_doc_appendix_d.px");
    std::fs::write(&px, &fence).expect("write the extracted fence");
    // The AoC-2024-day-1 sample the demo is built from; its answers are 11 / 31.
    let input = tmp.join("design_doc_appendix_d.in");
    std::fs::write(&input, "3   4\n4   3\n2   5\n1   3\n3   9\n3   3\n")
        .expect("write the sample input");

    let out = Command::new(bin_path())
        .arg("check")
        .arg(&px)
        .output()
        .expect("failed to run praxis");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        codes(&stderr).is_empty(),
        "`praxis check` on Appendix D's own fence must report no diagnostic, got:\n{stderr}\
         \n--- the fence ---\n{fence}"
    );

    let out = Command::new(bin_path())
        .arg("run")
        .arg(&px)
        .arg("--input")
        .arg(&input)
        .output()
        .expect("failed to run praxis");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "Appendix D must run against the input it is a demo of, got:\n{stderr}"
    );
    // The values, not merely a clean exit: `11` is the total distance and `31`
    // the similarity score for that sample.
    assert_eq!(stdout.trim(), "11\n31", "stderr:\n{stderr}");
}

/// **ADR-093.** `praxis check` and `praxis run` say the same thing about a
/// method that cannot resolve, and the thing they say is one `Y110`.
///
/// This asserts the pair of values, not merely that they agree — agreement is
/// also what "both silent" gives.
///
/// The three programs are the three ways a method call can fail to resolve: a
/// concrete receiver, a deferred receiver a call site pins, and a deferred
/// receiver nothing ever pins, whose constraint would otherwise sit in
/// `pending_constraints` forever.
///
/// The final assertion is the strong one: the two commands' stderr is
/// byte-identical, which is what "one emitter, before the fork" actually means.
#[test]
fn check_and_run_agree_about_a_method_that_cannot_resolve() {
    let tmp = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    for (label, src) in [
        ("concrete", "var v = Vec[Int]()\nv.push(1)\nout(v.nope())\n"),
        ("pinned", "fn f(x) { x.nope() }\nout(f(3))\n"),
        ("never_pinned", "fn f(x) { x.nope() }\nout(1)\n"),
    ] {
        let px = tmp.join(format!("divergence_{label}.px"));
        std::fs::write(&px, src).expect("write the program");
        let mut seen: Vec<String> = Vec::new();
        for command in ["check", "run"] {
            let out = Command::new(bin_path())
                .arg(command)
                .arg(&px)
                .output()
                .expect("failed to run praxis");
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            assert_eq!(
                codes(&stderr),
                vec!["Y110".to_string()],
                "`praxis {command}` on {label}:\n{src}\n{stderr}"
            );
            assert_eq!(out.status.code(), Some(1), "{label} / {command}");
            seen.push(stderr);
        }
        assert_eq!(
            seen[0], seen[1],
            "{label}: the two commands must say it identically"
        );
    }
}
