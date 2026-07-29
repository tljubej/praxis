//! Symbols: the identity layer of name resolution (§13.3).
//!
//! Each declaration (`let`/`var`/`fn`/param/builtin) gets a fresh [`SymbolId`].
//! Two bindings of the same source name in the same scope (shadowing) get
//! *distinct* ids — that is what lets the type checker, debugger, and hover tell
//! them apart (§4.2, §5.3). A [`Symbol`] records the name, kind, declaration
//! span, and (after inference) the inferred [`Scheme`].

use praxis_source::{FileSpan, Span};
use praxis_types::Scheme;

/// An opaque, interned identifier for one declaration. Every shadowing
/// declaration mints a new, distinct id.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SymbolId(pub u32);

impl SymbolId {
    #[inline]
    #[must_use]
    pub const fn to_u32(self) -> u32 {
        self.0
    }
}

/// What kind of declaration a symbol represents. Closed so the resolver and the
/// inferer can dispatch exhaustively.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SymbolKind {
    /// A `let name = …` binding. May be generalized (§5.3).
    Let,
    /// A `var name = …` binding. Never generalized; reassignable.
    Var,
    /// A `fn name(…) { … }` declaration.
    Fn,
    /// A function parameter `name: Type`.
    Param,
    /// A prelude/builtin *value* symbol (`out`, `panic`, `Vec`, …) seeded into
    /// the root scope.
    Builtin,
    /// A built-in scalar *type* name (`Int`, `Text`, …) seeded into the root
    /// scope. Distinct from [`Builtin`](Self::Builtin) because the prelude holds
    /// both, and only one of them may appear in type position: `out` is a name
    /// that resolves, and `let x: out = 1` was accepted on exactly that basis
    /// (TY-11).
    BuiltinType,
    /// A `struct Name { … }` declaration (M7, §4.5). A type-name symbol; its
    /// scheme carries the record's [`Type`](praxis_types::Type) once registered.
    Struct,
    /// An `enum Name { … }` declaration (M7, §4.6). A type-name symbol; its
    /// scheme carries the enum's [`Type`](praxis_types::Type) once registered.
    Enum,
}

impl SymbolKind {
    /// Whether a name bound to this kind denotes a **type**, and so may appear
    /// in type position.
    ///
    /// The complement is every kind that denotes a *value*: `let`, `var`, a
    /// parameter, a function, and the prelude's value builtins. Annotation
    /// validation used to ask only whether the name resolved at all, so
    /// `let Alias = 1` made `Alias` a legal annotation that silently named no
    /// type (TY-11).
    #[must_use]
    pub fn is_type(self) -> bool {
        matches!(
            self,
            SymbolKind::BuiltinType | SymbolKind::Struct | SymbolKind::Enum
        )
    }
}

/// One resolved declaration.
#[derive(Clone, Debug)]
pub struct Symbol {
    pub id: SymbolId,
    /// The source name as written (e.g. `a`). Shadowed bindings share a name but
    /// not an id.
    pub name: String,
    pub kind: SymbolKind,
    /// Where the binding is introduced. `None` for builtins with no source site.
    pub decl: Option<FileSpan>,
    /// The inferred type scheme. Filled in by type inference; `None` until then
    /// (and for symbols that fail to infer).
    pub scheme: Option<Scheme>,
}

impl Symbol {
    /// The declaration span alone (convenience for diagnostics), or a zero-width
    /// span at the file start if the symbol is a builtin.
    #[must_use]
    pub fn span(&self) -> Span {
        self.decl.map(|fs| fs.span).unwrap_or(Span::EMPTY)
    }
}
