//! Name resolution and the high-level intermediate representation (§13.3, §14.1).
//!
//! The HIR takes M1's lossless tree and layers two passes on top of it:
//!
//! 1. **Name resolution** ([`resolve`]): walk the typed AST, build a lexical
//!    scope tree, mint a [`SymbolId`] per declaration, resolve every name
//!    reference, and emit `N0xx` diagnostics. Shadowing is handled here — each
//!    `let`/`var` declaration gets a distinct id, and an initializer resolves
//!    names in the *preceding* environment (§4.2/§5.3).
//! 2. **Type inference** (the `infer` module, Slice 5): consume the resolved
//!    names and infer a [`Scheme`] for every expression and binding, emitting
//!    `Y0xx` diagnostics.
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
pub mod parser_lower;
pub mod resolve;
pub mod scope;
pub mod symbol;

pub use lower::{
    expr_span, expr_ty, lower, stmt_span, AssignOp, BinOp, Lit, TypedBlock, TypedExpr, TypedFn,
    TypedItem, TypedMatchArm, TypedModule, TypedParam, TypedPattern, TypedStmt, UnaryOp,
};
pub use name_table::NameTable;
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

/// The full result of analyzing one file: resolution + inference + diagnostics.
///
/// Built by [`analyze`]; consumed by the CLI (for diagnostics) and the LSP (for
/// hover/completion, in M11).
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
    /// [`SymbolId`] it mints. Survives shadowing (each `let`/`var`/fn`/param
    /// declaration is keyed by its own range). Consumed by M4 lowering.
    pub decls: std::collections::HashMap<rowan::TextRange, SymbolId>,
    /// Each *call site*, keyed by the callee name token's source range. Records
    /// the callee symbol and the concrete argument types the callee was called
    /// with (the instantiation witness). Consumed by monomorphization (WS8) to
    /// instantiate polymorphic callees per call site.
    pub call_sites: std::collections::HashMap<rowan::TextRange, CallSite>,
    /// All `N0xx` (name) and `Y0xx` (type) diagnostics, in source order.
    pub diagnostics: Vec<Diagnostic>,
}

/// One call site's monomorphization witness (WS8, §13.6): the callee symbol and
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
    let inference = infer::infer_with_tree(file, resolution, root);
    Analysis {
        db: inference.db,
        names: inference.names,
        scopes: inference.scopes,
        refs: inference.refs,
        ref_types: inference.ref_types,
        decls: inference.decls,
        call_sites: inference.call_sites,
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
#[path = "hir_tests.rs"]
mod hir_tests;

#[cfg(test)]
#[path = "hover_tests.rs"]
mod hover_tests;

#[cfg(test)]
#[path = "infer_tests.rs"]
mod infer_tests;
