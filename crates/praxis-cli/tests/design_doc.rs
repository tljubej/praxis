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

fn bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_praxis"))
}

/// The workspace root, from this crate's manifest directory.
fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates/praxis-cli -> crates
    p.pop(); // crates -> workspace root
    p
}

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
