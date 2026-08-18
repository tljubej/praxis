//! Tests for name resolution (§13.3).
//!
//! These cover the §19-M2 acceptance criteria that name resolution owns:
//! distinct symbol ids for shadowed bindings, and a shadowing initializer that
//! resolves to the *previous* binding. They parse a source fragment, resolve
//! it, and inspect the symbol table and reference map.

#![cfg(test)]

// The helpers every test module in this crate shares: the analyze/lower
// preamble and the lookups for a lowered item. They live beside the tests that
// use them, and the other modules reach them as `crate::hir_tests::test_util`.
#[path = "test_util.rs"]
pub(crate) mod test_util;

use std::collections::HashMap;

use praxis_source::DiagnosticCategory;
use rowan::TextRange;

use crate::{NameRef, ResolvedRef, SymbolKind};

use test_util::analyze;

/// Collect every resolved name reference (range → resolved ref) in source order.
fn refs(src: &str) -> (crate::Analysis, Vec<(TextRange, ResolvedRef)>) {
    let analysis = analyze(src);
    let mut refs: Vec<_> = analysis.refs.iter().map(|(r, v)| (*r, *v)).collect();
    refs.sort_by_key(|(r, _)| (u32::from(r.start()), u32::from(r.end())));
    (analysis, refs)
}

#[test]
fn shadowed_let_bindings_get_distinct_symbol_ids() {
    // §19-M2 criterion 2: `var a = 4; var a = "Foo"` — each occurrence resolves
    // to the correct symbol.
    let src = "var a = 4\nvar a = \"Foo\"";
    let (analysis, refs) = refs(src);
    // There are no name *references* here (both RHSes are literals), but two
    // *declarations* named `a`, with distinct ids.
    let a_symbols: Vec<_> = analysis
        .names
        .all()
        .iter()
        .filter(|s| s.name == "a")
        .collect();
    assert_eq!(a_symbols.len(), 2, "expected two `a` bindings");
    assert_ne!(a_symbols[0].id, a_symbols[1].id);
    // No references to resolve in this program.
    assert!(refs.is_empty());
    assert!(analysis.is_clean());
}

#[test]
fn shadowed_reference_resolves_to_latest_binding() {
    // After `var a = 4; var a = "Foo"`, a use of `a` resolves to the second.
    let src = "var a = 4\nvar a = \"Foo\"\nout(a)";
    let (analysis, refs) = refs(src);
    // Two references: `out` (a builtin) and `a`. Find the `a` one.
    let a_ref = refs
        .iter()
        .find(|(_, r)| analysis.names.get(r.symbol).is_some_and(|s| s.name == "a"))
        .expect("an `a` reference");
    let second_a = analysis
        .names
        .all()
        .iter()
        .filter(|s| s.name == "a")
        .nth(1)
        .expect("two `a` bindings");
    assert_eq!(
        a_ref.1.symbol, second_a.id,
        "reference should resolve to the latest `a`"
    );
    assert!(analysis.is_clean());
}

#[test]
fn shadowing_initializer_resolves_to_previous_binding() {
    // §19-M2 criterion 3: `var a = 4; var a = a + 1` — the RHS `a` is the FIRST.
    let src = "var a = 4\nvar a = a + 1";
    let (analysis, refs) = refs(src);
    assert_eq!(
        refs.len(),
        1,
        "expected the RHS `a` reference, got {refs:?}"
    );
    let (_range, r) = &refs[0];
    let first_a = analysis
        .names
        .all()
        .iter()
        .find(|s| s.name == "a")
        .expect("first `a`");
    assert_eq!(
        r.symbol, first_a.id,
        "shadowing initializer must resolve to the PREVIOUS binding"
    );
    assert!(analysis.is_clean());
}

#[test]
fn unresolved_name_emits_n001() {
    let analysis = analyze("out(missing)");
    let name_diags: Vec<_> = analysis
        .diagnostics
        .iter()
        .filter(|d| d.code().category() == DiagnosticCategory::Name)
        .collect();
    assert_eq!(name_diags.len(), 1);
    assert!(
        analysis.diagnostics[0]
            .message()
            .contains("`missing` is not defined")
    );
}

#[test]
fn reassignment_is_a_reference_not_a_declaration() {
    // `var x = 0; x = 1` — the `x` on the second line refers to the binding, it
    // does not mint a new symbol.
    let src = "var x = 0\nx = 1";
    let (analysis, refs) = refs(src);
    let x_decls: Vec<_> = analysis
        .names
        .all()
        .iter()
        .filter(|s| s.name == "x")
        .collect();
    assert_eq!(x_decls.len(), 1, "reassignment must not mint a new symbol");
    // One reference: the lhs `x` of the assignment.
    let x_refs: Vec<_> = refs
        .iter()
        .filter(|(_, r)| r.symbol == x_decls[0].id)
        .collect();
    assert_eq!(x_refs.len(), 1);
}

#[test]
fn function_parameters_bind_in_body_scope() {
    let src = "fn add(a, b) { a + b }";
    let (analysis, refs) = refs(src);
    // Both `a` and `b` are referenced once each in the body.
    assert_eq!(refs.len(), 2);
    // Parameters are bound as Param symbols.
    let params: Vec<_> = analysis
        .names
        .all()
        .iter()
        .filter(|s| s.kind == SymbolKind::Param)
        .collect();
    assert_eq!(params.len(), 2);
    assert!(analysis.is_clean());
}

#[test]
fn out_and_panic_are_in_prelude_scope() {
    // `out` and `panic` are builtins, seeded into the root scope.
    let src = "out(1)";
    let (analysis, refs) = refs(src);
    assert_eq!(refs.len(), 1);
    let out_symbol = analysis.names.get(refs[0].1.symbol).expect("out resolves");
    assert_eq!(out_symbol.name, "out");
    assert_eq!(out_symbol.kind, SymbolKind::Builtin);
}

#[test]
fn local_in_block_does_not_leak_outward() {
    // A `var` inside a block is not visible after the block.
    let src = "{ var inner = 1 }\nout(inner)";
    let analysis = analyze(src);
    // `inner` reference is unresolved.
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message().contains("`inner` is not defined"))
    );
}

#[test]
fn known_type_annotations_resolve_cleanly() {
    // `Int`, `Text`, `Bool`, `Unit`, `Never` are all known type names.
    let src = "var x: Int = 1\nvar s: Text = \"a\"";
    let analysis = analyze(src);
    assert!(
        analysis.is_clean(),
        "expected no diagnostics: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn unknown_type_annotation_emits_n002() {
    // `Byte` is reserved but not yet constructible (§4.3) → N002.
    let src = "var x: Byte = 1";
    let analysis = analyze(src);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.code().category() == DiagnosticCategory::Name
                && d.message().contains("unknown type `Byte`"))
    );
}

#[test]
fn float_type_annotation_resolves() {
    // `Float` is wired end-to-end (§4.12); the annotation resolves cleanly.
    let src = "var x: Float = 2.5";
    let analysis = analyze(src);
    assert!(
        analysis.is_clean(),
        "expected no diagnostics: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn recursive_function_can_call_itself() {
    // The function name is visible inside its own body (§4.9).
    let src = "fn f(n: Int) -> Int { f(n) }";
    let (analysis, _refs) = refs(src);
    assert!(
        analysis.is_clean(),
        "recursive call must resolve: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn references_keyed_by_range_form_a_map() {
    // Ensure the refs map is well-formed (no panic, all entries have ranges).
    let _ = NameRef {
        symbol: crate::SymbolId(0),
        range: TextRange::new(0u32.into(), 1u32.into()),
    };
    let src = "var a = 1\nout(a)";
    let (analysis, refs) = refs(src);
    assert!(!refs.is_empty());
    let _: HashMap<TextRange, ResolvedRef> = analysis.refs.clone();
}

/// §9.8: a `:bp` marker lowers to a stop **after** the statement it marks, not
/// before — which is what makes the binding it created visible in `locals`.
///
/// The ordering is the whole semantics of the marker, and it is a property of
/// the statement list rather than of any one statement's lowering, so this is
/// where it is pinned.
#[test]
fn a_breakpoint_marker_lowers_to_a_stop_after_its_statement() {
    use crate::{TypedExpr, TypedStmt};

    let (_, module) = test_util::analyze_and_lower("var a = 1 :bp\nvar b = 2");
    let entry = test_util::entry_fn(&module);
    let kinds: Vec<&str> = entry
        .body
        .stmts
        .iter()
        .map(|s| match s {
            TypedStmt::Var { .. } => "var",
            TypedStmt::Breakpoint { .. } => "bp",
            _ => "other",
        })
        .collect();
    assert_eq!(
        kinds,
        ["var", "bp", "var"],
        "the stop follows its statement"
    );

    // The span is the marker's own, so the debugger points at `:bp` and not at
    // the whole statement.
    let TypedStmt::Breakpoint { span } = entry.body.stmts[1] else {
        panic!("statement 1 is the stop");
    };
    assert_eq!(&"var a = 1 :bp"[span.0 as usize..span.1 as usize], ":bp");

    // A file's top-level statements have a synthesized `Unit` tail, so a marker
    // on the last line is a statement's and never the tail's.
    let (_, module) = test_util::analyze_and_lower("out(1) :bp");
    let entry = test_util::entry_fn(&module);
    assert!(
        entry.body.tail_bp.is_none(),
        "the entry tail is synthesized"
    );
    assert!(matches!(
        entry.body.stmts.last(),
        Some(TypedStmt::Breakpoint { .. })
    ));

    // A marker on a *block's* trailing expression cannot be a statement — the
    // tail is the block's value — so it rides the block instead, and the tail
    // is still the expression that was there.
    let (_, module) =
        test_util::analyze_and_lower("fn f() -> Int {\n  var m = 1\n  m + 1 :bp\n}\nout(f())");
    let f = test_util::fn_named(&module, "f");
    assert!(f.body.tail_bp.is_some(), "the marker rides the block");
    assert!(
        matches!(f.body.tail, TypedExpr::Bin { .. }),
        "the tail is `m + 1`"
    );
    assert!(
        !f.body
            .stmts
            .iter()
            .any(|s| matches!(s, TypedStmt::Breakpoint { .. })),
        "a tail marker is not also a statement"
    );

    // An unmarked program has neither.
    let (_, module) = test_util::analyze_and_lower("var a = 1\nout(a)");
    let entry = test_util::entry_fn(&module);
    assert!(entry.body.tail_bp.is_none());
    assert!(
        !entry
            .body
            .stmts
            .iter()
            .any(|s| matches!(s, TypedStmt::Breakpoint { .. }))
    );
}

/// A pending tail that turns out **not** to be the tail is demoted to a
/// statement — and its marker has to be demoted with it, in that order.
///
/// `{ out(1) :bp\n var a = 2 }` is the case: `out(1)` is recorded as the tail,
/// then the `var` arrives and pushes it down. A marker left behind on the block
/// would fire after `var a = 2` instead of after `out(1)`, which is the wrong
/// line and the wrong state.
#[test]
fn a_demoted_tails_marker_is_demoted_with_it() {
    use crate::TypedStmt;

    let (_, module) =
        test_util::analyze_and_lower("fn f() {\n  out(1) :bp\n  var a = 2\n  out(a)\n}\nf()");
    let f = test_util::fn_named(&module, "f");
    assert!(
        f.body.tail_bp.is_none(),
        "the marker did not stay on the block"
    );
    let kinds: Vec<&str> = f
        .body
        .stmts
        .iter()
        .map(|s| match s {
            TypedStmt::Expr(_) => "expr",
            TypedStmt::Var { .. } => "var",
            TypedStmt::Breakpoint { .. } => "bp",
            _ => "other",
        })
        .collect();
    assert_eq!(
        kinds,
        ["expr", "bp", "var"],
        "the stop stayed immediately after `out(1)`"
    );
}
