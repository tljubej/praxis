//! ADR-130's gate: coverage is decided by **analysis**, and the fix it offers
//! is a program that checks clean.
//!
//! Each assertion here names something an implementation could get plausibly
//! wrong and still pass a weaker test: reporting `Y120` from lowering (where
//! `praxis check` and the editor never see it), reporting one twice now that
//! two passes build patterns, inventing a `Y120` for a scrutinee whose type a
//! *later* line pins, and offering a fix whose text does not compile.

#![cfg(test)]

use praxis_ast::AstNode;
use praxis_parser::parse;
use praxis_source::{DiagCode, SourceMap};

use crate::analyze_root;

fn analyze(text: &str) -> crate::Analysis {
    let map = SourceMap::new();
    let id = map.intern("coverage_test.px", text);
    let parsed = parse(id, text);
    analyze_root(id, &parsed.tree)
}

fn codes(analysis: &crate::Analysis, want: DiagCode) -> Vec<&praxis_source::Diagnostic> {
    analysis
        .diagnostics
        .iter()
        .filter(|d| d.kind() == want)
        .collect()
}

/// **The milestone's reason for this pass.** A non-exhaustive match is reported
/// by `analyze` — which is what `praxis check` and the language server run —
/// and not only by lowering, which only `praxis run` reaches.
#[test]
fn a_non_exhaustive_match_is_reported_by_analysis() {
    let analysis =
        analyze("enum E { A, B }\nfn f(e: E) -> Int {\n    match e {\n        A => 1\n    }\n}\n");
    let found = codes(&analysis, DiagCode::NonExhaustiveMatch);
    assert_eq!(found.len(), 1, "{:?}", analysis.diagnostics);
    assert!(
        found[0].message().contains("`B`"),
        "the message names the missing variant: {}",
        found[0].message()
    );
}

/// …exactly once. Two passes build patterns now (this one and lowering), and
/// only one of them may report.
#[test]
fn it_is_reported_once() {
    let src = "enum E { A, B }\nfn f(e: E) -> Int {\n    match e {\n        A => 1\n    }\n}\n";
    let analysis = analyze(src);
    assert_eq!(codes(&analysis, DiagCode::NonExhaustiveMatch).len(), 1);

    // And lowering — the other builder — adds none of its own.
    let map = SourceMap::new();
    let id = map.intern("coverage_test.px", src);
    let parsed = parse(id, src);
    let mut analysis = analyze_root(id, &parsed.tree);
    let root = praxis_ast::SourceFile::cast(parsed.tree.clone()).expect("a source file");
    let module = crate::lower(id, &root, &mut analysis);
    assert_eq!(
        module
            .diagnostics
            .iter()
            .filter(|d| d.kind() == DiagCode::NonExhaustiveMatch)
            .count(),
        0,
        "lowering no longer decides coverage: {:?}",
        module.diagnostics
    );
}

/// An exhaustive match is silent, including through a payload — the case the
/// pre-matrix scan got wrong and the one a "did anything report" test cannot
/// tell from a false positive.
#[test]
fn an_exhaustive_match_is_silent() {
    let analysis = analyze(
        "enum Flag { On, Off }\nenum W { Wrap(Flag) }\n\
         fn f(w: W) -> Int {\n    match w {\n        Wrap(On) => 1\n        Wrap(Off) => 2\n    }\n}\n",
    );
    assert!(
        codes(&analysis, DiagCode::NonExhaustiveMatch).is_empty(),
        "{:?}",
        analysis.diagnostics
    );
}

/// **Why the pass runs after inference and not inside it.** The scrutinee here
/// is an unannotated parameter whose type only the *call* below pins. A check
/// run while `infer_match` was on the stack would see a type variable, find no
/// signature to enumerate, and demand a `_` arm the program does not need.
#[test]
fn a_scrutinee_pinned_later_in_the_file_is_not_reported() {
    let analysis = analyze(
        "enum E { A, B }\nfn f(e) -> Int {\n    match e {\n        A => 1\n        B => 2\n    }\n}\nout(f(A))\n",
    );
    assert!(
        codes(&analysis, DiagCode::NonExhaustiveMatch).is_empty(),
        "{:?}",
        analysis.diagnostics
    );
}

/// An arm an earlier arm already covers is `Y121`, from the same pass.
#[test]
fn an_unreachable_arm_is_reported_by_analysis() {
    let analysis = analyze(
        "enum E { A, B }\nfn f(e: E) -> Int {\n    match e {\n        _ => 0\n        A => 1\n    }\n}\n",
    );
    assert_eq!(
        codes(&analysis, DiagCode::UnreachableArm).len(),
        1,
        "{:?}",
        analysis.diagnostics
    );
}

/// **The fix is a program.** Applying the `Y120`'s machine-applicable
/// suggestion to the source produces a file that analyzes clean — which is the
/// only test that catches an arm whose pattern is rendered in a shape the
/// grammar does not accept, or a body that does not type-check.
#[test]
fn applying_the_suggested_arms_makes_the_file_clean() {
    for src in [
        "enum E { A, B, C }\nfn f(e: E) -> Int {\n    match e {\n        A => 1\n    }\n}\n",
        "enum Flag { On, Off }\nenum W { Wrap(Flag) }\n\
         fn f(w: W) -> Int {\n    match w {\n        Wrap(On) => 1\n    }\n}\n",
        // One line, so the fix has to invent an indent rather than copy one.
        "enum E { A, B }\nfn f(e: E) -> Int { match e { A => 1 } }\n",
    ] {
        let analysis = analyze(src);
        let found = codes(&analysis, DiagCode::NonExhaustiveMatch);
        assert_eq!(found.len(), 1, "{src}\n{:?}", analysis.diagnostics);
        let fix = found[0]
            .suggestions()
            .iter()
            .find(|s| s.replacement.is_some())
            .expect("the diagnostic carries a machine-applicable fix");
        let span = fix.span.span;
        let (start, end) = (span.start().to_u32() as usize, span.end().to_u32() as usize);
        let mut fixed = String::with_capacity(src.len());
        fixed.push_str(&src[..start]);
        fixed.push_str(fix.replacement.as_deref().unwrap_or_default());
        fixed.push_str(&src[end..]);

        let after = analyze(&fixed);
        assert!(
            after.diagnostics.is_empty(),
            "the fixed program must be clean.\n--- fixed ---\n{fixed}\n--- got ---\n{:?}",
            after
                .diagnostics
                .iter()
                .map(|d| format!("{} {}", d.code(), d.message()))
                .collect::<Vec<_>>()
        );
    }
}

/// A missing shape the type cannot enumerate — an `Int` scrutinee — gets **no**
/// fix. Writing the catch-all for the author would answer the question the
/// diagnostic is asking: what should happen to the rest?
#[test]
fn a_catch_all_witness_offers_no_fix() {
    let analysis = analyze("fn f(n: Int) -> Int {\n    match n {\n        1 => 1\n    }\n}\n");
    let found = codes(&analysis, DiagCode::NonExhaustiveMatch);
    assert_eq!(found.len(), 1, "{:?}", analysis.diagnostics);
    assert!(
        found[0]
            .suggestions()
            .iter()
            .all(|s| s.replacement.is_none()),
        "a `_` witness is not a fix"
    );
}

/// The generated arms take the file's own indentation, read from the arms that
/// are already there — a file indented with tabs gets tabs.
#[test]
fn the_fix_copies_the_files_indentation() {
    let src = "enum E { A, B }\nfn f(e: E) -> Int {\n\tmatch e {\n\t\tA => 1\n\t}\n}\n";
    let analysis = analyze(src);
    let found = codes(&analysis, DiagCode::NonExhaustiveMatch);
    assert_eq!(found.len(), 1, "{:?}", analysis.diagnostics);
    let replacement = found[0].suggestions()[0]
        .replacement
        .clone()
        .expect("a fix");
    assert_eq!(replacement, "\n\t\tB => panic(\"todo\")");
}
