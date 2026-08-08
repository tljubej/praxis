//! ADR-130's gate: coverage is decided by **analysis**, and the fix it offers
//! is a program that checks clean.
//!
//! Each assertion here names something an implementation could get plausibly
//! wrong and still pass a weaker test: reporting `Y120` from lowering (where
//! `praxis check` and the editor never see it), reporting one twice because two
//! passes build patterns, inventing a `Y120` for a scrutinee whose type a
//! *later* line pins, and offering a fix whose text does not compile.

#![cfg(test)]

use praxis_source::DiagCode;

use crate::hir_tests::test_util::{analyze, analyze_and_lower, parse_file};

fn codes(analysis: &crate::Analysis, want: DiagCode) -> Vec<&praxis_source::Diagnostic> {
    analysis
        .diagnostics
        .iter()
        .filter(|d| d.kind() == want)
        .collect()
}

/// A non-exhaustive match is reported by `analyze` — which is what
/// `praxis check` and the language server run — and not only by lowering, which
/// only `praxis run` reaches.
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

/// …exactly once. Two passes build patterns (this one and lowering), and only
/// one of them may report.
#[test]
fn it_is_reported_once() {
    let src = "enum E { A, B }\nfn f(e: E) -> Int {\n    match e {\n        A => 1\n    }\n}\n";
    let analysis = analyze(src);
    assert_eq!(codes(&analysis, DiagCode::NonExhaustiveMatch).len(), 1);

    // And lowering — the other builder — adds none of its own.
    let (_, module) = analyze_and_lower(src);
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

/// An exhaustive match is silent, including through a payload — the case a
/// "did anything report" test cannot tell from a false positive.
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

// --- ADR-133: the codes ADR-130 did not move -------------------------------

/// **The gate.** Every diagnostic a *well-formed* program can earn is analysis's,
/// because analysis is what `praxis check` and the editor run.
///
/// A diagnostic only lowering decides is one `Snapshot::diagnostics` never
/// reaches, so the file checks clean and then refuses to run — ADR-130's own
/// statement of the problem. These are the codes ADR-133 covers.
#[test]
fn a_program_run_refuses_is_a_program_check_refuses() {
    for (src, want) in [
        // `Y013` at an expression.
        (
            "var x = 99999999999999999999999\nout(x)\n",
            DiagCode::IntLiteralOutOfRange,
        ),
        // …and at a literal *pattern*, which the pattern builder decides.
        (
            "fn f(n: Int) -> Int { match n { 99999999999999999999 => 1, _ => 0 } }\nout(f(1))\n",
            DiagCode::IntLiteralOutOfRange,
        ),
        // `Y124`: `A(i, j)` on a one-slot variant.
        (
            "enum Bla { A(Int), B, C }\nvar bla = A(3)\nmatch bla { A(i, j) => {} B => {} C => {} }\n",
            DiagCode::PayloadArityMismatch,
        ),
        // `Y125` at a `for` header…
        (
            "struct Point { x: Int, y: Int }\nvar pts = [Point{x: 0, y: 1}]\n\
             for Point { x: 0, y } in pts {\n    out(y)\n}\n",
            DiagCode::RefutableBinding,
        ),
        // …and at a destructuring closure parameter, the third pattern position.
        (
            "var f = |Some(n)| n\nout(f(Some(1)))\n",
            DiagCode::RefutableBinding,
        ),
    ] {
        let analysis = analyze(src);
        assert!(
            !codes(&analysis, want).is_empty(),
            "{src}\nmust report {want:?} from analysis, got {:?}",
            analysis
                .diagnostics
                .iter()
                .map(|d| format!("{} {}", d.code(), d.message()))
                .collect::<Vec<_>>()
        );
    }
}

/// …and exactly once. Inference and the pattern builder walk the same patterns,
/// and the two codes they *both* decide — a variant the enum has not (`Y122`)
/// and a shape the scrutinee cannot take (`Y123`) — must not arrive twice.
#[test]
fn a_diagnostic_two_passes_agree_on_is_reported_once() {
    for (src, want) in [
        (
            "enum E { A, B }\nfn f(e: E) -> Int { match e { A => 1, Nope(x) => 2, B => 3 } }\n",
            DiagCode::UnknownEnumVariant,
        ),
        // A `for` header is the other walk, and it must agree the same way. A
        // *bare* `Nope` would be a binding, so this names one that can only be
        // a variant pattern.
        (
            "enum E { A, B }\nvar e = A\nfor Nope(x) in [e] { out(0) }\n",
            DiagCode::UnknownEnumVariant,
        ),
    ] {
        let analysis = analyze(src);
        assert_eq!(
            codes(&analysis, want).len(),
            1,
            "{src}\n{:?}",
            analysis
                .diagnostics
                .iter()
                .map(|d| format!("{} {}", d.code(), d.message()))
                .collect::<Vec<_>>()
        );
    }
}

/// **Every place the grammar puts a pattern is a place analysis checks one.**
///
/// The three positions — a match arm, a `for` header, a closure parameter — are
/// walked by hand across two passes, so a *fourth* one added to the grammar
/// would silently be checked by lowering alone and reintroduce this whole class.
/// This fails the moment the parser produces a top-level `PATTERN` anywhere
/// else; the fix is to walk it in `check_binding_patterns`, not to widen the set
/// here.
#[test]
fn every_pattern_position_is_checked_by_analysis() {
    use praxis_syntax::SyntaxKind;
    let src = "struct P { x: Int, y: Int }\n\
               enum E { A(Int), B }\n\
               var ps = [P { x: 1, y: 2 }]\n\
               for P { x, y } in ps { out(x + y) }\n\
               var f = |(a, b)| a + b\n\
               out(f((1, 2)))\n\
               var e = A(1)\n\
               out(match e { A(n) => n, B => 0 })\n";
    let (_, parsed) = parse_file(src);
    assert!(
        parsed.diagnostics.is_empty(),
        "the sample must parse: {:?}",
        parsed.diagnostics
    );

    let mut parents: Vec<SyntaxKind> = parsed
        .tree
        .descendants()
        .filter(|n| n.kind() == SyntaxKind::PATTERN)
        // Only the *outermost* pattern of each position: a sub-pattern is
        // reached by recursion, not by a walk.
        .filter(|n| {
            n.parent()
                .is_none_or(|p| !matches!(p.kind(), SyntaxKind::PATTERN | SyntaxKind::PATTERN_FIELD))
        })
        .filter_map(|n| n.parent().map(|p| p.kind()))
        .collect();
    parents.sort_by_key(|k| format!("{k:?}"));
    parents.dedup();
    assert_eq!(
        parents,
        vec![
            SyntaxKind::FOR_EXPR,
            SyntaxKind::MATCH_ARM,
            SyntaxKind::PARAM
        ],
        "a pattern position analysis does not walk"
    );
}

// --- the character literal (ADR-141) ---

/// `Char` falls through `signature()` to `Signature::Open` exactly as `Int`
/// does, so a `match` over one is never exhaustive without a `_` — the rule for
/// every scalar with more values than anyone can enumerate.
#[test]
fn a_match_on_a_char_needs_a_wildcard() {
    let analysis = analyze(
        "fn f(c: Char) -> Int {\n    match c {\n        '#' => 1\n        '.' => 2\n    }\n}\n",
    );
    let found = codes(&analysis, DiagCode::NonExhaustiveMatch);
    assert_eq!(found.len(), 1, "{:?}", analysis.diagnostics);
    assert_eq!(
        found[0].message(),
        "non-exhaustive match: missing a `_` catch-all arm"
    );

    // …and the same match *with* a `_` is clean, which is the half that stops
    // this from passing for the wrong reason.
    let analysis = analyze(
        "fn f(c: Char) -> Int {\n    match c {\n        '#' => 1\n        '.' => 2\n        _ => 0\n    }\n}\n",
    );
    assert!(
        analysis.diagnostics.is_empty(),
        "{:?}",
        analysis.diagnostics
    );
}

/// `LitKey::render` prints a `Char` witness as `'x'`, so the suggested arm is a
/// program the reader can paste.
#[test]
fn a_char_witness_renders_with_single_quotes() {
    let analysis = analyze(
        "enum W { Cell(Char) }\nfn f(w: W) -> Int {\n    match w {\n        Cell('#') => 1\n    }\n}\n",
    );
    let found = codes(&analysis, DiagCode::NonExhaustiveMatch);
    assert_eq!(found.len(), 1, "{:?}", analysis.diagnostics);
    let fix = found[0]
        .suggestions()
        .iter()
        .find_map(|s| s.replacement.as_deref())
        .expect("a machine-applicable fix");
    assert!(
        !fix.contains("\"#\""),
        "a Char witness is not a Text literal: {fix}"
    );
}

/// A repeated `'#'` arm is unreachable, which only works because `LitKey::Char`
/// compares by code point — the same equality the pattern test lowers to.
#[test]
fn a_repeated_char_arm_is_unreachable() {
    let analysis = analyze(
        "fn f(c: Char) -> Int {\n    match c {\n        '#' => 1\n        '#' => 2\n        _ => 0\n    }\n}\n",
    );
    assert_eq!(
        codes(&analysis, DiagCode::UnreachableArm).len(),
        1,
        "{:?}",
        analysis.diagnostics
    );

    // Two *different* characters are two arms, not one — the failure a key that
    // ignored its payload would produce.
    let analysis = analyze(
        "fn f(c: Char) -> Int {\n    match c {\n        '#' => 1\n        '.' => 2\n        _ => 0\n    }\n}\n",
    );
    assert!(
        codes(&analysis, DiagCode::UnreachableArm).is_empty(),
        "{:?}",
        analysis.diagnostics
    );
}
