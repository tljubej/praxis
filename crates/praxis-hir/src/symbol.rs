//! Symbols: the identity layer of name resolution (§13.3).
//!
//! Each declaration (`var`/`fn`/param/builtin) gets a fresh [`SymbolId`]. Two
//! bindings of the same source name in the same scope (shadowing) get
//! *distinct* ids — that is what lets the type checker, debugger, and hover tell
//! them apart (§4.2, §5.3). A [`Symbol`] records the name, kind, declaration
//! span, whether anything ever [reassigns](Symbol::reassigned) it, and (after
//! inference) the inferred [`Scheme`].

use praxis_source::{FileSpan, Span};
use praxis_types::Scheme;

/// An opaque, interned identifier for one declaration. Every shadowing
/// declaration mints a new, distinct id. One value is reserved and names no
/// declaration: [`SymbolId::UNRESOLVED`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SymbolId(pub u32);

impl SymbolId {
    /// The id that stands for *no declaration*. Two different situations reach
    /// it, and they are deliberately one value because a consumer holding an id
    /// can act on neither:
    ///
    /// - name resolution found nothing for a reference, so lowering has no
    ///   symbol to record (`lower_path`, `lower_call`);
    /// - the name is a *method*, which resolves to a catalog entry rather than
    ///   a declaration and so has no id by construction (HIR-02, `hover`).
    ///
    /// It is reserved rather than merely unused:
    /// [`NameTable::insert`](crate::NameTable::insert) mints ids from the
    /// table's length, so a real symbol reaches it only after `u32::MAX`
    /// declarations in one file.
    ///
    /// Naming it is the point. `names.get(UNRESOLVED)` returns `None`, exactly
    /// like a genuine miss does, so nothing downstream is forced to notice —
    /// and a callee that quietly carried it once lowered to a direct call
    /// through no function at all and took the host down (`fs.get(0)(100)`,
    /// M8 adversarial audit).
    pub const UNRESOLVED: SymbolId = SymbolId(u32::MAX);

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
    /// A value binding: `var name = …`, a `for` loop's variable, or a name a
    /// pattern introduces. All three are the same thing — a name bound to a
    /// value, reassignable like any other (ADR-125). Whether it *is* reassigned
    /// is [`Symbol::reassigned`], not a second kind.
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
    /// that resolves, and `var x: out = 1` was accepted on exactly that basis
    /// (TY-11).
    BuiltinType,
    /// A `struct Name { … }` declaration (M7, §4.5). A type-name symbol; its
    /// scheme carries the record's [`Type`](praxis_types::Type) once registered.
    Struct,
    /// An `enum Name { … }` declaration (M7, §4.6). A type-name symbol; its
    /// scheme carries the enum's [`Type`](praxis_types::Type) once registered.
    Enum,
    /// One variant of an `enum` declaration, as a *constructor* name in scope
    /// (§4.6): `Empty`, `Number(5)`. Its scheme is the constructor's — a `Func`
    /// returning the enum type for a payload variant, the enum type itself for
    /// a payload-less one.
    ///
    /// Distinct from [`Fn`](Self::Fn), which is what it used to be bound as,
    /// because "is this name a constructor" is otherwise only answerable by
    /// looking the *text* up in the root scope — and a local shadowing a
    /// variant answers yes (HIR-03). It is also not answerable from the scheme
    /// alone: `var A = Empty` has the enum type too.
    EnumVariant,
}

impl SymbolKind {
    /// Whether a name bound to this kind denotes a **type**, and so may appear
    /// in type position.
    ///
    /// The complement is every kind that denotes a *value*: `var`, a
    /// parameter, a function, and the prelude's value builtins. Annotation
    /// validation used to ask only whether the name resolved at all, so
    /// `var Alias = 1` made `Alias` a legal annotation that silently named no
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
    /// Whether any `name = …` statement anywhere in the file writes **this**
    /// binding. Set by name resolution, which is the one pass that already
    /// answers "which declaration does this target name reach" and so is the
    /// only place that can tell a write to a shadowed binding from a write to
    /// the one that shadowed it.
    ///
    /// It is the replacement for the `let`/`var` distinction (ADR-125), and it
    /// carries the two consequences that distinction used to carry:
    ///
    /// - **Generalization** (§5.3). A binding nothing reassigns may be
    ///   generalized under the value restriction; a reassigned one may not.
    ///   Generalizing a reassigned binding is unsound — the assignment
    ///   *instantiates* the scheme rather than constraining it, so
    ///   `var f = |x| x` followed by `f = |n| n + 1` would leave `f` at
    ///   `forall T. T -> T` and `f("s")` would call the `Int` closure.
    /// - **Capture representation** (§4.10). A captured binding needs a shared
    ///   `VarCell` only if something can write it; otherwise the value is
    ///   copied into the closure's environment.
    ///
    /// Note that a *place* assignment (`v[i] = x`, `p.f = x`) is not a
    /// reassignment: it mutates the object the binding points at and leaves the
    /// binding itself alone.
    pub reassigned: bool,
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
