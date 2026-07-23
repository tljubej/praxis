//! Tests for name resolution (§13.3) — the Slice 4 layer.
//!
//! These cover the §19-M2 acceptance criteria that name resolution owns:
//! distinct symbol ids for shadowed bindings, and a shadowing initializer that
//! resolves to the *previous* binding. (The type-inference criteria land with
//! Slice 5.) They parse a source fragment, resolve it, and inspect the symbol
//! table and reference map.

#![cfg(test)]

use std::collections::HashMap;

use praxis_parser::parse;
use praxis_source::{DiagnosticCategory, SourceMap};
use rowan::TextRange;

use crate::{analyze_root, NameRef, ResolvedRef, SymbolKind};

fn resolve_src(text: &str) -> crate::Analysis {
    let map = SourceMap::new();
    let id = map.intern("hir_test.px", text);
    let parsed = parse(id, text);
    analyze_root(id, &parsed.tree)
}

/// Collect every resolved name reference (range → resolved ref) in source order.
fn refs(src: &str) -> (crate::Analysis, Vec<(TextRange, ResolvedRef)>) {
    let analysis = resolve_src(src);
    let mut refs: Vec<_> = analysis.refs.iter().map(|(r, v)| (*r, *v)).collect();
    refs.sort_by_key(|(r, _)| (u32::from(r.start()), u32::from(r.end())));
    (analysis, refs)
}

#[test]
fn shadowed_let_bindings_get_distinct_symbol_ids() {
    // §19-M2 criterion 2: `let a = 4; let a = "Foo"` — each occurrence resolves
    // to the correct symbol.
    let src = "let a = 4\nlet a = \"Foo\"";
    let (analysis, refs) = refs(src);
    // Two references? No — there are no name *references* here (the RHSes are
    // literals). But there are two *declarations* named `a`, with distinct ids.
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
    // After `let a = 4; let a = "Foo"`, a use of `a` resolves to the second.
    let src = "let a = 4\nlet a = \"Foo\"\nout(a)";
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
    // §19-M2 criterion 3: `let a = 4; let a = a + 1` — the RHS `a` is the FIRST.
    let src = "let a = 4\nlet a = a + 1";
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
    let analysis = resolve_src("out(missing)");
    let name_diags: Vec<_> = analysis
        .diagnostics
        .iter()
        .filter(|d| d.code().category() == DiagnosticCategory::Name)
        .collect();
    assert_eq!(name_diags.len(), 1);
    assert!(analysis.diagnostics[0]
        .message()
        .contains("`missing` is not defined"));
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
    // A `let` inside a block is not visible after the block.
    let src = "{ let inner = 1 }\nout(inner)";
    let analysis = resolve_src(src);
    // `inner` reference is unresolved.
    assert!(analysis
        .diagnostics
        .iter()
        .any(|d| d.message().contains("`inner` is not defined")));
}

#[test]
fn known_type_annotations_resolve_cleanly() {
    // `Int`, `Text`, `Bool`, `Unit`, `Never` are all known type names.
    let src = "let x: Int = 1\nlet s: Text = \"a\"";
    let analysis = resolve_src(src);
    assert!(
        analysis.is_clean(),
        "expected no diagnostics: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn unknown_type_annotation_emits_n002() {
    // `Float` is reserved but not constructible in M2 (§4.3) → N002.
    let src = "let x: Float = 1";
    let analysis = resolve_src(src);
    assert!(analysis
        .diagnostics
        .iter()
        .any(|d| d.code().category() == DiagnosticCategory::Name
            && d.message().contains("unknown type `Float`")));
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
    let src = "let a = 1\nout(a)";
    let (analysis, refs) = refs(src);
    assert!(!refs.is_empty());
    let _: HashMap<TextRange, ResolvedRef> = analysis.refs.clone();
}
