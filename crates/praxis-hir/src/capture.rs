//! Closure capture analysis (§4.10).
//!
//! A closure captures the free variables in its body — name references that
//! resolve to bindings declared *outside* the closure (in an enclosing scope).
//! Each capture becomes one slot in the closure's runtime environment
//! ([`Capture`]), shared by the allocation site, the synthetic function's entry
//! prologue, and (for mutable captures) the `VarCell` cell.
//!
//! ## Detection
//!
//! The analysis walks the closure's body subtree and collects every name
//! reference whose symbol is a value binding (a `var`, a `for` variable, a
//! pattern name, or a param) **declared outside** the closure body. "Outside"
//! is decided by the declaration range: the closure's own params and any `var`
//! it introduces are declared at ranges *inside* the body's text range, so they
//! are locals, not captures.
//! `fn`/`struct`/`enum`/`builtin` references are never captures (a `fn` call is
//! a static call; the others are type names).
//!
//! Nested closure bodies **are** descended into, and must be. A name a nested
//! closure references that resolves *outside this* closure is a capture of this
//! closure as well as of the nested one: the nested closure's environment is
//! filled from this closure's frame at the point its literal is allocated, so
//! this closure has to be holding the value in order to hand it over. The
//! nested closure's own params and locals are declared at ranges inside
//! `closure_range` and are filtered by the same test that filters this
//! closure's own — so descending cannot over-capture.
//!
//! The result preserves first-seen order, which becomes the env slot index
//! shared across lowering, the synthetic function prologue, and the allocation.
//!
//! Whether a capture is a copy or a shared cell is *not* decided here: this
//! module answers which symbols are captured, and lowering pairs each with the
//! binding's [`reassigned`](crate::Symbol::reassigned) flag to pick a
//! [`CaptureKind`] (ADR-125).

use std::collections::HashMap;

use praxis_ast::Expr;
use rowan::{NodeOrToken, TextRange};

use crate::resolve::ResolvedRef;
use crate::symbol::{SymbolId, SymbolKind};

/// How a single captured variable is represented in the closure's environment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureKind {
    /// A capture of a binding nothing ever reassigns: the value is copied into
    /// the closure's env at allocation time and bound to a fresh local in the
    /// synthetic function's prologue. Nothing can write it on either side, so
    /// the copy can never be observed to be one.
    ByValue,
    /// A capture of a **reassigned** binding: the env slot holds a `VarCell` GC
    /// cell shared between the binding site and the closure. Reads/writes in the
    /// body go through the cell, so a write on either side is seen by both.
    ByCell,
}

/// One captured variable: its symbol, source name, static type, and how it is
/// represented in the environment. The order of captures in the analysis
/// result is the env slot index.
#[derive(Clone, Debug)]
pub struct Capture {
    pub symbol: SymbolId,
    pub name: String,
    pub ty: praxis_typeck::Type,
    pub kind: CaptureKind,
}

/// A free-variable capture found during the walk: the symbol and its kind. The
/// caller resolves the type and source name from the symbol table.
#[derive(Clone, Copy, Debug)]
pub struct FreeVar {
    pub symbol: SymbolId,
    pub kind: SymbolKind,
    /// The source range of the name reference that first discovered this
    /// capture. Lowering looks the inferred type of the capture up by this
    /// range.
    pub ref_range: TextRange,
}

/// The result of capture analysis: the ordered list of free-variable symbols.
///
/// A mutable capture is not an error; it is an ordinary capture that lowering
/// gives a `CaptureKind::ByCell` env slot.
#[derive(Debug, Default)]
pub struct CaptureAnalysis {
    pub captures: Vec<FreeVar>,
}

/// Walk `body` collecting free-variable captures.
///
/// - `refs`: the resolved-reference map (range → `ResolvedRef`).
/// - `closure_range`: the text range of the *whole closure node* (params +
///   body). A binding whose declaration range is *inside* this range is a local
///   of the closure (a param or a closure-local `var`), not a capture.
/// - `decl_range(symbol) -> Option<TextRange>`: looks up a symbol's declaration
///   range (from the analysis `decls` map). `None` (builtins have no source site)
///   means "not a capture".
/// - `kind_of(symbol) -> Option<SymbolKind>`: classifies a referenced symbol so
///   value bindings are distinguished from `fn`/`struct`/`enum`/`builtin`
///   references (which are never captures).
pub fn analyze<R, K>(
    body: &Expr,
    closure_range: TextRange,
    refs: &HashMap<TextRange, ResolvedRef>,
    mut decl_range: R,
    mut kind_of: K,
) -> CaptureAnalysis
where
    R: FnMut(SymbolId) -> Option<TextRange>,
    K: FnMut(SymbolId) -> Option<SymbolKind>,
{
    let mut seen: Vec<SymbolId> = Vec::new();
    let mut out = CaptureAnalysis::default();
    walk(
        body,
        closure_range,
        refs,
        &mut decl_range,
        &mut kind_of,
        &mut seen,
        &mut out,
    );
    out
}

/// A **flat** scan of every token in the body's subtree — nested closures
/// included, which is the point. `descendants_with_tokens()` already yields the
/// whole subtree, and `record_free_var`'s "declared outside `closure_range`"
/// test is the entire predicate: it keeps exactly the names that must live in
/// this closure's environment and drops every binding either closure declares.
///
/// This function is not recursive and never calls itself; `analyze` is its only
/// caller. In particular it does not stop at an expression that *is* a nested
/// closure: `|a| |b| b + base` must record `base`, or the inner closure's
/// environment is filled from an empty one.
fn walk<R, K>(
    expr: &Expr,
    closure_range: TextRange,
    refs: &HashMap<TextRange, ResolvedRef>,
    decl_range: &mut R,
    kind_of: &mut K,
    seen: &mut Vec<SymbolId>,
    out: &mut CaptureAnalysis,
) where
    R: FnMut(SymbolId) -> Option<TextRange>,
    K: FnMut(SymbolId) -> Option<SymbolKind>,
{
    // Scan every token in the subtree. A name reference is *any* resolved NAME
    // token: a PathExpr name (a read), an AssignStmt target (a write), etc. —
    // both are captures if they resolve to an outer value binding. The resolver
    // records every resolved name in `refs`, keyed by its token range, so we
    // simply check membership.
    for child in expr.syntax().descendants_with_tokens() {
        if let NodeOrToken::Token(t) = child {
            // Tokens inside a nested closure are scanned *deliberately*: see the
            // module doc. Nothing here excludes them, and nothing should.
            let range = t.text_range();
            if let Some(rref) = refs.get(&range) {
                record_free_var(*rref, range, closure_range, decl_range, kind_of, seen, out);
            }
        }
    }
}

/// Record one free variable if it is a value binding declared outside the body.
fn record_free_var<R, K>(
    rref: ResolvedRef,
    ref_range: TextRange,
    closure_range: TextRange,
    decl_range: &mut R,
    kind_of: &mut K,
    seen: &mut Vec<SymbolId>,
    out: &mut CaptureAnalysis,
) where
    R: FnMut(SymbolId) -> Option<TextRange>,
    K: FnMut(SymbolId) -> Option<SymbolKind>,
{
    let kind = kind_of(rref.symbol);
    // Only value bindings are captures; `fn`/`struct`/`enum`/`builtin` references
    // are static (a direct call or a type name), not captured.
    let is_value = matches!(kind, Some(SymbolKind::Var) | Some(SymbolKind::Param));
    if !is_value {
        return;
    }
    // A capture must be declared *outside* the closure body. Params and
    // closure-local `var`s are declared at ranges inside `closure_range`;
    // outer bindings are declared outside. Builtins have no decl range (None) —
    // they are not captures.
    let Some(decl) = decl_range(rref.symbol) else {
        return;
    };
    if closure_range.contains_range(decl) {
        // Declared inside the body → a local of the closure, not a capture.
        return;
    }
    if seen.contains(&rref.symbol) {
        return;
    }
    seen.push(rref.symbol);
    let kind = kind.unwrap_or(SymbolKind::Var);
    out.captures.push(FreeVar {
        symbol: rref.symbol,
        kind,
        ref_range,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use praxis_ast::{AstNode, SourceFile};
    use praxis_parser::parse;
    use praxis_source::SourceMap;

    /// Run name resolution + capture analysis on a single closure, returning the
    /// captured symbols' source names in order.
    fn analyze_closures(src: &str) -> CaptureAnalysis {
        let map = SourceMap::new();
        let id = map.intern("capture_test.px", src);
        let parsed = parse(id, src);
        let root = SourceFile::cast(parsed.tree.clone()).unwrap();
        let resolution = crate::resolve::resolve(id, &root);
        // Find the closure expression in the tree.
        let closure = parsed
            .tree
            .descendants()
            .find_map(praxis_ast::ClosureExpr::cast)
            .expect("test source must contain a closure");
        let body = closure.body().expect("closure has a body");
        // The "inside the closure" boundary is the whole closure node (params +
        // body), so a param or closure-local binding is recognized as local.
        let closure_range = closure.syntax().text_range();
        let names = &resolution.names;
        let decls = &resolution.decls;
        analyze(
            &body,
            closure_range,
            &resolution.refs,
            |sym| decls.iter().find(|(_, s)| **s == sym).map(|(r, _)| *r),
            |sym| names.get(sym).map(|s| s.kind),
        )
    }

    fn capture_names(src: &str) -> Vec<String> {
        let names = &analyze_closures(src)
            .captures
            .iter()
            .map(|fv| fv.symbol)
            .collect::<Vec<_>>();
        // Re-resolve names via a fresh resolution (the analysis owns names; borrow
        // dance — simpler to just redo the lookup inline).
        let map = SourceMap::new();
        let id = map.intern("capture_test.px", src);
        let parsed = parse(id, src);
        let root = SourceFile::cast(parsed.tree.clone()).unwrap();
        let resolution = crate::resolve::resolve(id, &root);
        names
            .iter()
            .map(|sym| {
                resolution
                    .names
                    .get(*sym)
                    .map(|s| s.name.clone())
                    .unwrap_or_default()
            })
            .collect()
    }

    #[test]
    fn no_captures_for_pure_closure() {
        let names = capture_names("fn main() { var f = |x| x + 1; f(2) }");
        assert!(names.is_empty(), "expected no captures, got {names:?}");
    }

    #[test]
    fn captures_outer_let() {
        let names = capture_names("fn main() { var o = 10; var f = |x| x + o; f(2) }");
        assert_eq!(names, vec!["o"]);
    }

    #[test]
    fn captures_multiple_in_first_seen_order() {
        let names =
            capture_names("fn main() { var a = 1; var b = 2; var f = |x| x + a * b; f(2) }");
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn dedups_repeated_captures() {
        let names = capture_names("fn main() { var o = 10; var f = |x| o + o; f(2) }");
        assert_eq!(names, vec!["o"]);
    }

    #[test]
    fn closure_param_is_not_a_capture() {
        // The closure's own param `x` is declared inside the body; it must not be
        // captured.
        let names = capture_names("fn main() { var f = |x| x + x; f(2) }");
        assert!(
            names.is_empty(),
            "param should not be captured, got {names:?}"
        );
    }

    /// An inner closure's *own* bindings are never the outer closure's captures,
    /// even though the scan reads them: `x` is the outer's param and `y` is the
    /// inner's, and both are declared inside the outer closure node's range, so
    /// `record_free_var`'s `contains_range` test drops them. What survives is
    /// `o`, declared outside both — which the outer must hold in order to fill
    /// the inner's environment. A nested closure's captures are *not* separate
    /// from the enclosing closure's when they resolve outside them both.
    #[test]
    fn an_inner_closures_own_bindings_are_not_the_outers_captures() {
        let names = capture_names(
            "fn main() { var o = 10; var f = |x| { var g = |y| x + y; g(o) }; f(2) }",
        );
        assert!(
            !names.contains(&"x".to_string()),
            "param `x` should not be a capture, got {names:?}"
        );
        assert!(
            names.contains(&"o".to_string()),
            "expected `o` capture, got {names:?}"
        );
    }

    /// A captured binding is an ordinary capture, whatever declared it.
    ///
    /// The *kind* does not select `ByCell`: since ADR-125 the kind is `Var` for
    /// every binding, and what picks a cell is whether something reassigns it —
    /// a fact this module deliberately does not read (it is
    /// `Symbol::reassigned`, applied in lowering).
    #[test]
    fn a_mutable_capture_is_an_ordinary_capture() {
        // Note: `|| c` would parse as `PIPE2` (logical or), so the closure takes
        // a dummy `_` param to be a real closure literal.
        let analysis = analyze_closures("fn main() { var c = 0; var f = |_| c; f(0) }");
        assert_eq!(analysis.captures.len(), 1, "the var is captured");
        assert_eq!(analysis.captures[0].kind, SymbolKind::Var);
    }

    /// A parameter, a `for` variable and a pattern name are captured on the same
    /// terms as a `var` — which matters more since ADR-125, because each of them
    /// can now be written and so can be the one that needs a shared cell.
    #[test]
    fn a_for_variable_is_captured_like_any_other_binding() {
        let names = capture_names("fn main() { for i in 0..3 { var f = |_| i; f(0) } }");
        assert!(
            names.contains(&"i".to_string()),
            "expected the loop variable captured, got {names:?}"
        );
    }

    // --- Transitive captures -------------------------------------------------
    //
    // A closure whose body *is* a closure must capture what the inner one names
    // from outside them both, because the inner closure's environment is filled
    // from the outer's frame.

    #[test]
    fn a_closure_whose_body_is_a_closure_captures_transitively() {
        let names =
            capture_names("fn main() { var base = 10; var mk = |a| |b| b + base; mk(5)(1) }");
        assert_eq!(names, vec!["base"]);
    }

    /// Two programs that differ by one pair of braces cannot have different
    /// environments.
    #[test]
    fn a_braced_body_and_a_bare_body_capture_the_same_thing() {
        let bare =
            capture_names("fn main() { var base = 10; var mk = |a| |b| b + base; mk(5)(1) }");
        let braced =
            capture_names("fn main() { var base = 10; var mk = |a| { |b| b + base }; mk(5)(1) }");
        assert_eq!(bare, braced);
        assert_eq!(bare, vec!["base"]);
    }

    #[test]
    fn three_levels_of_nesting_capture_transitively() {
        let names = capture_names(
            "fn main() { var base = 10; var mk = |a| |b| |c| c + base; mk(1)(2)(3) }",
        );
        assert_eq!(names, vec!["base"]);
    }

    /// The over-capture guard: descending into the nested closure must not drag
    /// the nested closure's own param in. `b` is declared inside the outer
    /// closure's range, so it is a local of the outer closure, not a capture.
    #[test]
    fn an_inner_closures_param_is_not_the_outers_capture() {
        let names = capture_names("fn main() { var mk = |a| |b| b + a; mk(5)(1) }");
        assert!(names.is_empty(), "expected no captures, got {names:?}");
    }

    #[test]
    fn an_inner_closure_local_is_not_the_outers_capture() {
        let names = capture_names(
            "fn main() { var base = 1; var mk = |a| |b| { var t = b + base; t }; mk(5)(1) }",
        );
        assert_eq!(names, vec!["base"]);
    }

    /// A name the inner closure *shadows* is not a capture: the reference
    /// resolves to the inner param, whose declaration is inside `closure_range`.
    #[test]
    fn a_shadowing_inner_param_is_not_a_capture() {
        let names =
            capture_names("fn main() { var base = 10; var mk = |a| |base| base + 1; mk(5)(1) }");
        assert!(names.is_empty(), "expected no captures, got {names:?}");
    }

    /// First-seen order is the env slot index, shared by the allocation site and
    /// the synthetic function's prologue — so a transitive capture's order has to
    /// be pinned just like a direct one's.
    #[test]
    fn transitive_captures_keep_first_seen_order() {
        let names =
            capture_names("fn main() { var a = 1; var b = 2; var mk = |x| |y| b + a; mk(0)(0) }");
        assert_eq!(names, vec!["b", "a"]);
    }

    // --- names inside interpolation holes (§8.1, ADR-147) -------------------
    //
    // **This module is why interpolation is lexed the way it is.** `walk` scans
    // `descendants_with_tokens()` and looks each token's *range* up in the
    // resolver's `refs` map, so a name is a capture only if it is a real token
    // at a real range in the lossless tree. An implementation that kept
    // `"{outer}"` as one opaque `TextLit` and re-lexed the hole in a later pass
    // answers `[]` here — and the closure is then allocated with an empty
    // environment and reads a slot nothing filled, which is a wrong answer at
    // run time rather than a diagnostic. Nothing about that is specific to this
    // file; the tests are here because this is the module that would be wrong.

    /// **The gate for ADR-147 decision 1.**
    #[test]
    fn a_hole_in_a_closure_body_captures_the_name_it_holds() {
        let names = capture_names(r#"fn main() { var outer = 1; var f = |_| "{outer}"; f(0) }"#);
        assert_eq!(names, vec!["outer"]);
    }

    /// A hole holds a full expression, so every name in it is a capture on the
    /// same terms — and in first-seen order, because that order is the env slot
    /// index.
    #[test]
    fn every_name_in_a_hole_is_captured_in_first_seen_order() {
        let names =
            capture_names(r#"fn main() { var a = 1; var b = 2; var f = |_| "{b} {a}"; f(0) }"#);
        assert_eq!(names, vec!["b", "a"]);
    }

    /// The over-capture guard, one brace deeper: the closure's own param is
    /// declared inside the closure's range whether it is named in a hole or in
    /// ordinary expression position, so it is a local either way.
    #[test]
    fn a_param_named_in_a_hole_is_not_a_capture() {
        let names = capture_names(r#"fn main() { var f = |x| "{x}"; f(1) }"#);
        assert!(names.is_empty(), "expected no captures, got {names:?}");
    }

    /// A literal with no hole in it names nothing, so it captures nothing. The
    /// negative half matters: it is what says the fragments themselves are not
    /// being mistaken for names.
    #[test]
    fn a_literal_with_no_hole_captures_nothing() {
        let names = capture_names(r#"fn main() { var outer = 1; var f = |_| "outer"; f(0) }"#);
        assert!(names.is_empty(), "expected no captures, got {names:?}");
    }

    /// Two programs that differ only by whether the name is written inside a
    /// hole cannot have different environments. This is the interpolation form
    /// of `a_braced_body_and_a_bare_body_capture_the_same_thing`.
    #[test]
    fn a_name_in_a_hole_and_a_bare_name_capture_the_same_thing() {
        let bare = capture_names("fn main() { var o = 10; var f = |_| o; f(0) }");
        let in_hole = capture_names(r#"fn main() { var o = 10; var f = |_| "{o}"; f(0) }"#);
        assert_eq!(bare, in_hole);
        assert_eq!(bare, vec!["o"]);
    }

    #[test]
    fn captures_assigned_var_in_block_body() {
        // A `var` assigned inside the closure body (the lhs of `+=` is a bare
        // NAME token, not a PathExpr) is a capture.
        let names = capture_names(
            "fn main() { var total = 100; var add = |n| { total += n }; add(5); total }",
        );
        assert!(
            names.contains(&"total".to_string()),
            "expected `total` capture, got {names:?}"
        );
    }
}
