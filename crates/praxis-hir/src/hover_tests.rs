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
    let src = "var a = 4\nvar a = \"Foo\"\nout(a)";
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
    // Hover at the declaration site of a `var` shows its inferred type.
    let src = "var x = 1";
    let analysis = analyze(src);
    // The declaration `x` is at range 4..5 ("let x").
    let decl_range = TextRange::new(4u32.into(), 5u32.into());
    let hover = analysis.hover_decl(decl_range).expect("decl hover");
    assert_eq!(hover.name, "x");
    assert_eq!(hover.scheme, "Int");
}

#[test]
fn hover_distinguishes_two_shadowed_declarations() {
    let src = "var a = 4\nvar a = \"Foo\"";
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
    // `out`'s scheme is the polymorphic `forall T. (T) -> Unit` (the element
    // type is quantified, not pinned to the call site's Int — so the hover shows
    // the generalized scheme, matching how `out` is actually typed).
    let out_hover = analysis
        .refs
        .keys()
        .find_map(|r| {
            let h = analysis.hover(*r)?;
            (h.name == "out").then_some(h)
        })
        .expect("out hoverable");
    assert!(
        out_hover.scheme.contains("forall"),
        "expected the polymorphic scheme, got {}",
        out_hover.scheme
    );
    assert!(
        out_hover.scheme.contains("Unit"),
        "got {}",
        out_hover.scheme
    );
}

#[test]
fn hover_at_empty_range_returns_none() {
    let analysis = analyze("var x = 1");
    assert!(analysis
        .hover(TextRange::new(100u32.into(), 101u32.into()))
        .is_none());
}

/// **HIR-02.** Hover over a method name reports what the method call produces.
///
/// It used to report nothing at all: [`Analysis::hover`] looks the range up in
/// `refs` first, and a method name resolves to a catalog entry rather than to a
/// symbol, so it is not in `refs` and never will be. The result inference had
/// computed was written into `ref_types` at that same range — a map only
/// reference consumers read, and one whose entry was the *receiver*'s type
/// rather than the result's.
#[test]
fn hover_over_a_method_name_reports_its_result_type() {
    let src = "fn main(v: Vec[Int]) -> Int { v.len() }\n";
    let map = SourceMap::new();
    let id = map.intern("hover_method.px", src);
    let parsed = praxis_parser::parse(id, src);
    let analysis = analyze_root(id, &parsed.tree);
    let len_tok = parsed
        .tree
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.text() == "len")
        .expect("the `len` token");
    let info = analysis
        .hover(len_tok.text_range())
        .expect("a method name is hoverable");
    assert_eq!(info.scheme, "Int", "`Vec[Int].len()` produces an Int");
    assert_eq!(info.name, "Vec[Int].len");
}

/// …and hovering a *name* still answers about the name. Giving methods their
/// own map must not turn a same-ranged reference into a method.
#[test]
fn hover_over_a_receiver_still_reports_the_binding() {
    let src = "fn main(v: Vec[Int]) -> Int { v.len() }\n";
    let map = SourceMap::new();
    let id = map.intern("hover_receiver.px", src);
    let parsed = praxis_parser::parse(id, src);
    let analysis = analyze_root(id, &parsed.tree);
    let v_use = analysis
        .refs
        .keys()
        .find_map(|r| {
            let h = analysis.hover(*r)?;
            (h.name == "v").then_some(h)
        })
        .expect("the `v` reference");
    assert_eq!(v_use.scheme, "Vec[Int]");
}
