//! Hover query tests: the hover query returns the inferred type and symbol
//! identity for each shadowed occurrence (§19-M2 criterion 5).
//!
//! These drive the library-level [`Analysis::hover`] query directly, without
//! the LSP.

#![cfg(test)]

use rowan::TextRange;

use crate::hir_tests::test_util::{analyze, parse_file};
use crate::{analyze_root, hover::HoverInfo};

#[test]
fn hover_over_shadowed_occurrences_returns_distinct_symbols() {
    // Two `a` bindings (Int then Text) and a use of the second.
    let src = "var a = 4\nvar a = \"Foo\"\nout(a)";
    let analysis = analyze(src);
    // The two `a` *declarations* are in `decls`; hover here works over
    // references. The single `a` reference (in `out(a)`) resolves to the second
    // binding (Text).
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
    // The declaration `x` is at range 4..5 (the `x` of `var x`).
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

/// Hover over a method name reports what the method call produces — the
/// *result* type, not the receiver's.
///
/// A method name resolves to a catalog entry rather than to a symbol, so it is
/// never in `refs`; [`Analysis::hover`] answers it from `method_refs` instead.
#[test]
fn hover_over_a_method_name_reports_its_result_type() {
    let src = "fn main(v: Vec[Int]) -> Int { v.len() }\n";
    let (id, parsed) = parse_file(src);
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
    let (id, parsed) = parse_file(src);
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

/// A parameter of a generic `fn` is a monotype whose variable that `fn`'s
/// scheme quantified. Both hover doors name it the way the `fn` does — `T`, not
/// the `?T` that means "nothing binds this" and reads as a failed inference.
///
/// The program is fully inferred: `foo` is `forall T. (() -> T) -> T` and
/// monomorphization pins `T` at the call site. Only the rendering was ever in
/// question.
#[test]
fn a_generic_fns_parameter_is_named_by_the_scheme_that_bound_it() {
    let src = "fn foo(c) {\n  c()\n}\n\nvar u = || {}\nfoo(u)\n";
    let analysis = analyze(src);
    // The declaration `c` of `fn foo(c)`.
    let decl = TextRange::new(7u32.into(), 8u32.into());
    let at_decl = analysis.hover_decl(decl).expect("the parameter's decl");
    assert_eq!(at_decl.name, "c");
    assert_eq!(at_decl.scheme, "() -> T");
    // …and the use of it in the body answers the same type.
    let at_use = analysis
        .refs
        .keys()
        .find_map(|r| {
            let h = analysis.hover(*r)?;
            (h.name == "c").then_some(h)
        })
        .expect("the `c` reference");
    assert_eq!(at_use.scheme, "() -> T");
    // The owning `fn` still shows its own quantifier.
    let foo_decl = TextRange::new(3u32.into(), 6u32.into());
    assert_eq!(
        analysis.hover_decl(foo_decl).expect("fn foo").scheme,
        "forall T. (() -> T) -> T"
    );
}

/// The `?` is not deleted, only moved off the bindings that never earned it: a
/// variable no scheme quantifies still renders with it.
#[test]
fn a_variable_no_scheme_quantifies_still_renders_unbound() {
    // An expansive binding is not generalized (§5.3 value restriction) and
    // nothing here pins its element, so the variable is owned by no scheme at
    // all — which is exactly what `?` reports.
    let src = "var v = Vec()\n";
    let analysis = analyze(src);
    let decl = TextRange::new(4u32.into(), 5u32.into());
    let at_decl = analysis.hover_decl(decl).expect("the binding's decl");
    assert_eq!(at_decl.name, "v");
    assert!(
        at_decl.scheme.contains('?'),
        "an unowned variable keeps its `?`, got {}",
        at_decl.scheme
    );
}
