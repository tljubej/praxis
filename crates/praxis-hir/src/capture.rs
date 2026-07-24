//! Closure capture analysis (M7-WS7, §4.10).
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
//! reference whose symbol is a value binding (`let`/`var`/`param`) **declared
//! outside** the closure body. "Outside" is decided by the declaration range:
//! the closure's own params and any `let`/`var` it introduces are declared at
//! ranges *inside* the body's text range, so they are locals, not captures.
//! `fn`/`struct`/`enum`/`builtin` references are never captures (a `fn` call is
//! a static call; the others are type names).
//!
//! Nested closure bodies are *not* descended into — their free variables are
//! their own captures, not this closure's.
//!
//! The result preserves first-seen order, which becomes the env slot index
//! shared across lowering, the synthetic function prologue, and the allocation.
//!
//! For M7-WS7a (this module) only immutable captures (`let`/`param`) are
//! supported; a `var` capture is reported as a `Y130` diagnostic until WS7b
//! lands the `VarCell` runtime cell.

use std::collections::HashMap;

use praxis_ast::Expr;
use rowan::{NodeOrToken, TextRange};

use crate::resolve::ResolvedRef;
use crate::symbol::{SymbolId, SymbolKind};

/// How a single captured variable is represented in the closure's environment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureKind {
    /// An immutable capture (`let` binding or `param`): the value is copied into
    /// the closure's env at allocation time and bound to a fresh local in the
    /// synthetic function's prologue.
    ByValue,
    /// A mutable capture (`var` binding): the env slot holds a `VarCell` GC cell
    /// shared between the binding site and the closure. Reads/writes in the body
    /// go through the cell (WS7b).
    ByCell,
}

/// One captured variable: its symbol, source name, static type, and how it is
/// represented in the environment. The order of captures in the analysis
/// result is the env slot index.
#[derive(Clone, Debug)]
pub struct Capture {
    pub symbol: SymbolId,
    pub name: String,
    pub ty: praxis_types::Type,
    pub kind: CaptureKind,
}

/// A free-variable capture found during the walk: the symbol and its kind. The
/// caller resolves the type and source name from the symbol table.
#[derive(Clone, Copy, Debug)]
pub struct FreeVar {
    pub symbol: SymbolId,
    pub kind: SymbolKind,
    /// The source range of the name reference that first discovered this capture.
    /// Used to anchor diagnostics (e.g. `Y130` for an unsupported mutable capture).
    pub ref_range: TextRange,
}

/// The result of capture analysis: the ordered list of free-variable symbols
/// plus any diagnostics (e.g. an unsupported `var` capture for WS7a).
#[derive(Debug, Default)]
pub struct CaptureAnalysis {
    pub captures: Vec<FreeVar>,
    pub errors: Vec<CaptureError>,
}

/// A capture-analysis diagnostic, with the symbol and the source range to report
/// at.
#[derive(Debug)]
pub struct CaptureError {
    pub symbol: SymbolId,
    pub range: TextRange,
    /// `true` for an unsupported mutable capture (WS7a); the diagnostic number
    /// is `Y130`.
    pub mutable_unsupported: bool,
}

/// Walk `body` collecting free-variable captures.
///
/// - `refs`: the resolved-reference map (range → `ResolvedRef`).
/// - `closure_range`: the text range of the *whole closure node* (params +
///   body). A binding whose declaration range is *inside* this range is a local
///   of the closure (a param or a closure-local `let`/`var`), not a capture.
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

/// The recursive walker. Descends into every expression kind *except* nested
/// closures (their captures are their own). Records each free value binding.
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
    // A nested closure manages its own captures — do not descend into it.
    if matches!(expr, Expr::Closure(_)) {
        return;
    }
    // A name reference (Path): record it if it resolves to an outer value binding.
    if let Expr::Path(p) = expr {
        if let Some(name_tok) = p.name() {
            let range = name_tok.text_range();
            if let Some(rref) = refs.get(&range) {
                record_free_var(*rref, range, closure_range, decl_range, kind_of, seen, out);
            }
        }
    }
    // Recurse into child expressions of every kind (skipping nested closures,
    // handled by the early return above).
    let self_range = expr.syntax().text_range();
    for child in expr.syntax().descendants_with_tokens() {
        if let NodeOrToken::Node(n) = child {
            if n.text_range() == self_range {
                continue;
            }
            if let Some(child_expr) = Expr::cast(n) {
                walk(
                    &child_expr,
                    closure_range,
                    refs,
                    decl_range,
                    kind_of,
                    seen,
                    out,
                );
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
    let is_value = matches!(
        kind,
        Some(SymbolKind::Let) | Some(SymbolKind::Var) | Some(SymbolKind::Param)
    );
    if !is_value {
        return;
    }
    // A capture must be declared *outside* the closure body. Params and
    // closure-local `let`/`var` are declared at ranges inside `closure_range`;
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
    let kind = kind.unwrap_or(SymbolKind::Let);
    if matches!(kind, SymbolKind::Var) {
        // WS7a: mutable captures are not yet supported. Record the error so the
        // lowerer can emit `Y130`; the capture is still recorded so downstream
        // lowering (once WS7b lands) has the full set.
        out.errors.push(CaptureError {
            symbol: rref.symbol,
            range: ref_range,
            mutable_unsupported: true,
        });
    }
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
        let names = capture_names("fn main() { let f = |x| x + 1; f(2) }");
        assert!(names.is_empty(), "expected no captures, got {names:?}");
    }

    #[test]
    fn captures_outer_let() {
        let names = capture_names("fn main() { let o = 10; let f = |x| x + o; f(2) }");
        assert_eq!(names, vec!["o"]);
    }

    #[test]
    fn captures_multiple_in_first_seen_order() {
        let names =
            capture_names("fn main() { let a = 1; let b = 2; let f = |x| x + a * b; f(2) }");
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn dedups_repeated_captures() {
        let names = capture_names("fn main() { let o = 10; let f = |x| o + o; f(2) }");
        assert_eq!(names, vec!["o"]);
    }

    #[test]
    fn closure_param_is_not_a_capture() {
        // The closure's own param `x` is declared inside the body; it must not be
        // captured.
        let names = capture_names("fn main() { let f = |x| x + x; f(2) }");
        assert!(
            names.is_empty(),
            "param should not be captured, got {names:?}"
        );
    }

    #[test]
    fn nested_closure_captures_are_separate() {
        // The outer closure captures `o`; the inner closure `|y| x + y` captures
        // `x` (the outer's param) but that is the inner's concern. The outer's
        // captures should be just `o` (and `x` must NOT appear, since it is the
        // outer's own param).
        let names = capture_names(
            "fn main() { let o = 10; let f = |x| { let g = |y| x + y; g(o) }; f(2) }",
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

    #[test]
    fn mutable_capture_records_error() {
        // Note: `|| c` would parse as `PIPE2` (logical or), so the closure takes
        // a dummy `_` param to be a real closure literal.
        let analysis = analyze_closures("fn main() { var c = 0; let f = |_| c; f(0) }");
        assert_eq!(analysis.captures.len(), 1, "the var is still recorded");
        assert!(
            analysis.errors.iter().any(|e| e.mutable_unsupported),
            "expected a mutable-capture error"
        );
    }
}
