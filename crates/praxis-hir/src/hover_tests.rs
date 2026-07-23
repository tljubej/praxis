//! Hover query tests (§19-M2 criterion 5).
//!
//! Criterion 5: "hover query returns the inferred type and symbol identity for
//! each shadowed occurrence." The real LSP is M11; here we exercise the
//! library-level [`Analysis::hover`] query directly.

#![cfg(test)]

use praxis_parser::parse;
use praxis_source::SourceMap;
use rowan::TextRange;

use crate::{analyze_root, hover::HoverInfo};

fn analyze(text: &str) -> crate::Analysis {
    let map = SourceMap::new();
    let id = map.intern("hover_test.px", text);
    let parsed = parse(id, text);
    analyze_root(id, &parsed.tree)
}

#[test]
fn hover_over_shadowed_occurrences_returns_distinct_symbols() {
    // Two `a` bindings (Int then Text) and a use of the second.
    let src = "let a = 4\nlet a = \"Foo\"\nout(a)";
    let analysis = analyze(src);
    // Find the reference ranges for the two `a` *declarations* are in decls, but
    // hover works over references. The single `a` reference (in `out(a)`)
    // resolves to the second binding (Text).
    let a_refs: Vec<(TextRange, HoverInfo)> = analysis
        .refs
        .keys()
        .filter_map(|r| {
            let info = analysis.hover(*r)?;
            if info.name == "a" {
                Some((*r, info))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(a_refs.len(), 1, "one `a` reference");
    let (_, hover) = &a_refs[0];
    // It resolved to the second `a` binding, whose type is Text.
    assert_eq!(hover.scheme, "Text");
}

#[test]
fn hover_over_declaration_shows_its_scheme() {
    // Hover at the declaration site of a `let` shows its inferred type.
    let src = "let x = 1";
    let analysis = analyze(src);
    // The declaration `x` is at range 4..5 ("let x").
    let decl_range = TextRange::new(4u32.into(), 5u32.into());
    let hover = analysis.hover_decl(decl_range).expect("decl hover");
    assert_eq!(hover.name, "x");
    assert_eq!(hover.scheme, "Int");
}

#[test]
fn hover_distinguishes_two_shadowed_declarations() {
    let src = "let a = 4\nlet a = \"Foo\"";
    let analysis = analyze(src);
    // First `a` decl at 4..5, second at 14..15.
    let first = analysis
        .hover_decl(TextRange::new(4u32.into(), 5u32.into()))
        .expect("first decl");
    let second = analysis
        .hover_decl(TextRange::new(14u32.into(), 15u32.into()))
        .expect("second decl");
    assert_ne!(first.symbol, second.symbol, "distinct symbol ids");
    assert_eq!(first.scheme, "Int");
    assert_eq!(second.scheme, "Text");
}

#[test]
fn hover_over_out_shows_polymorphic_scheme() {
    let src = "out(1)";
    let analysis = analyze(src);
    // The `out` reference's instantiated type at this call is (Int) -> Unit.
    let out_hover = analysis
        .refs
        .keys()
        .find_map(|r| {
            let h = analysis.hover(*r)?;
            (h.name == "out").then_some(h)
        })
        .expect("out hoverable");
    assert!(out_hover.scheme.contains("Int"), "got {}", out_hover.scheme);
    assert!(
        out_hover.scheme.contains("Unit"),
        "got {}",
        out_hover.scheme
    );
}

#[test]
fn hover_at_empty_range_returns_none() {
    let analysis = analyze("let x = 1");
    assert!(analysis
        .hover(TextRange::new(100u32.into(), 101u32.into()))
        .is_none());
}
