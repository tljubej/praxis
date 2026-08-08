//! Name resolution and the high-level intermediate representation (§13.3, §14.1).
//!
//! The HIR takes the lossless tree and layers two passes on top of it:
//!
//! 1. **Name resolution** ([`resolve`]): walk the typed AST, build a lexical
//!    scope tree, mint a [`SymbolId`] per declaration, resolve every name
//!    reference, and emit `N0xx` diagnostics. Shadowing is handled here — each
//!    `var` declaration gets a distinct id, and an initializer resolves
//!    names in the *preceding* environment (§4.2/§5.3).
//! 2. **Type inference** (the [`infer`] module): consume the resolved names and
//!    infer a [`Scheme`] for every expression and binding, emitting `Y0xx`
//!    diagnostics.
//!
//! The single entry point is [`analyze`], which runs both passes and returns an
//! [`Analysis`] carrying the symbol table, scope tree, resolved references,
//! inferred types, and all diagnostics.

pub mod capability;
pub mod capture;
pub mod catalog;
pub(crate) mod decl;
pub mod diagnostics;
pub mod exhaustive;
pub mod hover;
pub mod infer;
pub mod lower;
pub mod mono;
pub mod name_table;
pub mod parser_index;
pub mod parser_lower;
pub(crate) mod pattern;
pub mod resolve;
pub mod scope;
pub mod symbol;

pub use lower::{
    entry_point, expr_span, expr_ty, lower, stmt_exprs, stmt_span, AssignOp, BinOp, Lit,
    TypedBlock, TypedExpr, TypedFn, TypedItem, TypedMatchArm, TypedModule, TypedParam,
    TypedPattern, TypedStmt, UnaryOp, ENTRY_NAME,
};
pub use name_table::NameTable;
pub use parser_index::{CaptureAt, ParserIndex, ParserMode};
/// The identity of a compiled parser plan, re-exported so MIR can name the
/// field on `TypedExpr::Read`/`Parse` without depending on the input-parser
/// crate directly.
pub use praxis_input_parser::PlanId;
pub use resolve::{NameRef, NameResolution, ResolvedRef};
pub use scope::{ScopeId, ScopeTree};
pub use symbol::{Symbol, SymbolId, SymbolKind};

use praxis_ast::{AstNode, SourceFile};
use praxis_source::{Diagnostic, FileId};
use praxis_types::{Type, TypeDb};

/// The identity of one syntax **node**.
///
/// Deliberately a distinct type from a token's [`rowan::TextRange`]: a
/// `PATH_EXPR` node and the `Ident` token inside it have the *same* range, so a
/// range-keyed map cannot hold both "the type of this expression" and "the type
/// of this name reference" without one silently overwriting the other. Carrying
/// the node's [`SyntaxKind`](praxis_syntax::SyntaxKind) alongside the range
/// makes that collision unrepresentable (F15).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct NodeKey(rowan::TextRange, praxis_syntax::SyntaxKind);

impl NodeKey {
    /// The key of `node`.
    #[must_use]
    pub fn of(node: &praxis_syntax::SyntaxNode) -> NodeKey {
        NodeKey(node.text_range(), node.kind())
    }

    /// The node's source range.
    #[must_use]
    pub fn range(self) -> rowan::TextRange {
        self.0
    }

    /// The node's syntax kind.
    #[must_use]
    pub fn kind(self) -> praxis_syntax::SyntaxKind {
        self.1
    }
}

/// One resolved method call: the catalog entry it selected and the types
/// inference gave its receiver and result.
///
/// A method-name token is **not** a name reference, so this has a map of its
/// own rather than riding `ref_types`: that map is walked by every reference
/// consumer, and hover looks a range up in `refs` first and would never reach a
/// method call recorded there (HIR-02).
#[derive(Clone, Copy, Debug)]
pub struct MethodRef {
    /// The catalog entry the receiver/name/arity selected.
    pub entry: &'static praxis_stdlib::MethodEntry,
    /// The receiver's inferred type at this call site.
    pub receiver: Type,
    /// The result type inference gave this call.
    pub result: Type,
}

/// The full result of analyzing one file: resolution + inference + diagnostics.
///
/// Built by [`analyze`]; consumed by the CLI (for diagnostics) and the LSP (for
/// hover/completion).
#[derive(Debug)]
pub struct Analysis {
    /// The interned type arena, holding every type minted during inference.
    pub db: TypeDb,
    /// Every resolved symbol.
    pub names: NameTable,
    /// The lexical scope tree.
    pub scopes: ScopeTree,
    /// Each name reference, keyed by its source range, with the symbol it
    /// resolved to and the scope at the reference site.
    pub refs: std::collections::HashMap<rowan::TextRange, ResolvedRef>,
    /// The inferred type for each name reference's range (filled by inference).
    pub ref_types: std::collections::HashMap<rowan::TextRange, Type>,
    /// Each *declaration* site, keyed by the name token's source range → the
    /// [`SymbolId`] it mints. Survives shadowing (each `var`/`fn`/param
    /// declaration is keyed by its own range). Consumed by lowering.
    pub decls: std::collections::HashMap<rowan::TextRange, SymbolId>,
    /// Each *call site*, keyed by the callee name token's source range. Records
    /// the callee symbol and the concrete argument types the callee was called
    /// with (the instantiation witness). Consumed by monomorphization to
    /// instantiate polymorphic callees per call site.
    pub call_sites: std::collections::HashMap<rowan::TextRange, CallSite>,
    /// **Every** inferred expression's type, keyed by its node (F15). Filled at
    /// one insertion point, so "an expression inference visited that has no
    /// recorded type" cannot arise — which is what lets lowering *read* a type
    /// instead of re-deriving one at a second, independent instantiation.
    pub expr_types: std::collections::HashMap<NodeKey, Type>,
    /// Each method call, keyed by the method-name token's range.
    pub method_refs: std::collections::HashMap<rowan::TextRange, MethodRef>,
    /// Each `read`/`parse` body's retained parser AST and per-node types
    /// (ADR-098), in the order inference reached them. **The only data source**
    /// for hover on an inner constructor, capture-type completion, the four
    /// parser semantic-token classes, and §15.3's cursor-mode question — the
    /// alternative being a second scanner over template interiors living in the
    /// language server.
    pub parser_exprs: Vec<ParserIndex>,
    /// All `N0xx` (name) and `Y0xx` (type) diagnostics, in source order.
    pub diagnostics: Vec<Diagnostic>,
}

/// One call site's monomorphization witness (§13.6): the callee symbol and
/// the concrete argument types at the call site. After inference, the arg types
/// pin the callee's quantified type variables to concrete types, so the mono
/// pass can instantiate the callee's scheme with them.
#[derive(Clone, Debug)]
pub struct CallSite {
    /// The callee's symbol (a `SymbolKind::Fn` for user fns; builtins are also
    /// recorded so `Vec[T]()` can honor the element type).
    pub callee: SymbolId,
    /// The concrete argument types, in call order.
    pub arg_types: Vec<Type>,
    /// The call's result type at this site — the instantiation witness for a
    /// callee whose result mentions a quantified variable no *argument* does
    /// (`fn empty() { Vec() }`, MONO-02). Without it a zero-argument generic
    /// call site carries nothing to specialize from.
    pub result: Type,
}

impl Analysis {
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.diagnostics.is_empty()
    }
}

/// Run name resolution and type inference on `file`'s parsed tree.
///
/// `parse` (from `praxis-parser`) must already have run; pass its root here.
/// Never panics on malformed input — the parser guarantees a well-formed tree,
/// and unresolved names / type errors surface as diagnostics.
#[must_use]
pub fn analyze(file: FileId, root: &SourceFile) -> Analysis {
    let resolution = resolve::resolve(file, root);
    let mut inference = infer::infer_with_tree(file, resolution, root);
    // Coverage last (ADR-130): a scrutinee's type is not final until the whole
    // file has been inferred, and this is what puts `Y120`/`Y121` in front of
    // `praxis check` and the editor rather than only in front of `praxis run`.
    exhaustive::check_matches(
        file,
        root,
        &mut inference.db,
        &inference.decls,
        &inference.expr_types,
        &mut inference.diagnostics,
    );
    // The other two pattern positions (ADR-133). A `for` header and a
    // destructuring closure parameter are checked here for the same reason a
    // match arm is: lowering asked, and lowering is the pass `check` and the
    // editor do not run.
    pattern::check_binding_patterns(
        file,
        root,
        &mut inference.db,
        &inference.names,
        &inference.decls,
        &inference.ref_types,
        &mut inference.diagnostics,
    );
    Analysis {
        db: inference.db,
        names: inference.names,
        scopes: inference.scopes,
        refs: inference.refs,
        ref_types: inference.ref_types,
        decls: inference.decls,
        call_sites: inference.call_sites,
        expr_types: inference.expr_types,
        method_refs: inference.method_refs,
        parser_exprs: inference.parser_exprs,
        diagnostics: inference.diagnostics,
    }
}

/// Convenience: analyze the root of an already-parsed tree given the raw node.
/// Casts to [`SourceFile`] internally.
#[must_use]
pub fn analyze_root(file: FileId, root: &praxis_syntax::SyntaxNode) -> Analysis {
    match SourceFile::cast(root.clone()) {
        Some(sf) => analyze(file, &sf),
        None => Analysis {
            db: TypeDb::new(),
            names: NameTable::default(),
            scopes: ScopeTree::new(),
            refs: std::collections::HashMap::new(),
            ref_types: std::collections::HashMap::new(),
            decls: std::collections::HashMap::new(),
            call_sites: std::collections::HashMap::new(),
            expr_types: std::collections::HashMap::new(),
            method_refs: std::collections::HashMap::new(),
            parser_exprs: Vec::new(),
            // The parser should always produce a SOURCE_FILE root; if not, this
            // is an internal error, surfaced as a single diagnostic.
            diagnostics: vec![praxis_source::Diagnostic::new(
                praxis_source::Severity::Error,
                praxis_source::DiagCode::InternalNotASourceFile,
                "internal: parse tree root is not a SOURCE_FILE",
                praxis_source::FileSpan::new(file, praxis_source::Span::EMPTY),
            )],
        },
    }
}

#[cfg(test)]
#[path = "coverage_tests.rs"]
mod coverage_tests;

#[cfg(test)]
#[path = "hir_tests.rs"]
mod hir_tests;

#[cfg(test)]
#[path = "hover_tests.rs"]
mod hover_tests;

#[cfg(test)]
#[path = "infer_tests.rs"]
mod infer_tests;

#[cfg(test)]
#[path = "parser_index_tests.rs"]
mod parser_index_tests;
