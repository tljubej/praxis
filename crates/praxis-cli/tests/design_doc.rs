//! The design document's own programs, run through the real binary (REP-28).
//!
//! # Why this file exists
//!
//! REP-28 was signed off on a *paraphrase*. The finding named §4.9's fence; the
//! fix was gated on a synthetic program with a call site the fence does not have,
//! and the report counted the two `Y112`s the paraphrase emits rather than the
//! four the fence does. Both numbers were true of some program and only one of
//! them was the document's, so a fence that still passed `praxis check` and then
//! failed under `praxis run` was recorded as closed.
//!
//! So the fence is not retyped here. It is **extracted from
//! `praxis_technical_design.md` at test time**, byte for byte, and driven through
//! the same two commands a reader would type. A test that quotes the doc can
//! drift from it; a test that reads it cannot.
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
/// a gate that silently covers nothing is what this file is a correction for.
fn fence_after(doc: &str, heading: &str) -> String {
    let mut lines = doc.lines();
    lines
        .by_ref()
        .find(|l| l.trim() == heading)
        .unwrap_or_else(|| panic!("`{heading}` is not a heading in praxis_technical_design.md"));
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

/// **REP-28.** §4.9's fence, as the document writes it, compiles under both
/// `praxis check` and `praxis run`.
///
/// Before the fix this was the whole finding in one program: `check` exited 0 with
/// no output, and `run` exited 1 with four `Y112`s — `no field `x`` twice and
/// `no field `y`` twice, one per read on the line. Nothing calls `manhattan`, so
/// nothing ever says what `a` and `b` are, and lowering demanded a record
/// definition that no call site exists to supply.
#[test]
fn section_4_9s_function_example_checks_and_runs() {
    let root = workspace_root();
    let doc = std::fs::read_to_string(root.join("praxis_technical_design.md"))
        .expect("praxis_technical_design.md at the workspace root");
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

/// **REP-58 / ADR-090.** §7.7's "repeated labeled blocks" fence, as the document
/// writes it, runs against the input it describes.
///
/// It did not. `  Starting items: {items:csv(int)}` ends in its capture, so the
/// capture was its template's last part, and `block` — the one sequencing
/// construct that computed no window — handed `csv` the rest of the *section*.
/// `csv` read `79` and `98` and then faulted on the following five lines.
///
/// **Observed red** on the unpatched binary: `praxis run` on the extracted fence
/// exits 1 with `error: program faulted: input parse mismatch / at input offset
/// 34..149: expected the rest of the field`. It exits 0 on the patched one.
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
    let doc = std::fs::read_to_string(root.join("praxis_technical_design.md"))
        .expect("praxis_technical_design.md at the workspace root");
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

/// **REP-34.** Every ```praxis fence in the design document **parses**.
///
/// Four did not, all of them §7.5 parser-constructor calls with a labelled
/// argument: `chars(one_of("^v<>"), skip: whitespace)`, `sections(block(ranges:
/// …))`, `choice(Number: …)` and `scan(choice(Multiply: …))`, each a `P001` at
/// the `:`. The question the finding raised was which side was wrong, and the
/// answer is the document's. A labelled argument *is* implemented — the parser
/// emits `PARSER_NAMED_ARG` and `parser_lower` consumes it — but only inside the
/// **parser-expression sublanguage**, which is entered by `read` or by
/// `parse(text, …)` and nowhere else (§7.1). Written bare, those four fences were
/// ordinary call expressions, where `skip:` is a syntax the language does not
/// have. §7.5's own first four fences already wrote `read`; the rest did not, and
/// three more of them parsed only by coincidence, as calls to undefined names.
/// So every §7.5 fence says `read` now and §7.5 states why.
///
/// The sweep is the point, not the four. A per-fence assertion would have to be
/// written once per fence and would cover the ones someone remembered; this
/// covers the document, including fences added after it.
///
/// **Parse only.** A fence is a fragment — most of them name a `struct` declared
/// three sections earlier, or a binding the surrounding prose supplies — so
/// `N0xx` and `Y0xx` are expected and are not what this asserts. `P0xx` is
/// different in kind: it means the text is not the language, whatever the
/// context around it would have been.
#[test]
fn every_praxis_fence_in_the_design_doc_parses() {
    let root = workspace_root();
    let doc = std::fs::read_to_string(root.join("praxis_technical_design.md"))
        .expect("praxis_technical_design.md at the workspace root");

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
                "praxis_technical_design.md:{}: {parse_errors:?}\n{body}",
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

/// **REP-33 / ADR-093.** Appendix D — the document's own "first end-to-end demo
/// target" — checks clean, runs, and prints the answers it is a demo of.
///
/// It did neither. `praxis check` exited 0 with no output and `praxis run`
/// exited 1 with **eight** `Y110`s: `sorted` twice, then `zip`, `map`, `sum`,
/// `frequencies`, `map`, `sum`. Only two of the eight were real — `sorted` and
/// `frequencies` had no catalog row — and the other six were cascade off the
/// fresh type variable an unresolved call hands back. `zip`, `map`-on-a-pipeline
/// and `sum` were registered and working all along.
///
/// Two halves landed to make this pass, and the intermediate state was measured
/// rather than assumed: after ADR-093 moved `Y110` into inference, `check`
/// reported **three** — two `sorted` and one `frequencies`, the cascade gone.
/// The three §6.3 barrier rows then took it to zero.
///
/// The fence is extracted, not retyped: a test that quotes the doc can drift
/// from it, which is this file's whole reason for existing.
#[test]
fn appendix_ds_demo_checks_runs_and_prints_its_answer() {
    let root = workspace_root();
    let doc = std::fs::read_to_string(root.join("praxis_technical_design.md"))
        .expect("praxis_technical_design.md at the workspace root");
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
/// also what "both silent" gives, and both silent is what the tree did.
/// **Observed red**, all three programs: `codes(check_stderr)` was `[]` and
/// `codes(run_stderr)` was `["Y110"]`.
///
/// The three are the three ways a method call can fail to resolve, and each was
/// silent at `check` for its own reason: a concrete receiver whose miss was left
/// to lowering, a deferred receiver a call site pinned, and a deferred receiver
/// nothing ever pinned, whose constraint sat in `pending_constraints` forever.
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
