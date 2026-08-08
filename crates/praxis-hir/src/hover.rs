//! Hover query: given a source position, return the inferred type and symbol
//! identity of the name there (§15.2 hover, §19-M2 criterion 5).
//!
//! The real LSP lands in M11; for M2 this is a library-level query exercised by
//! a test, satisfying "hover returns the inferred type and symbol identity for
//! each shadowed occurrence."

use praxis_syntax::span_bridge::range_to_span;
use rowan::TextRange;

use crate::{Analysis, SymbolId};

/// What hover at a position reveals about a name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HoverInfo {
    /// The symbol the name resolves to. Two shadowed bindings with the same
    /// source name have *different* ids — this is how hover distinguishes them.
    ///
    /// `None` where the hovered thing is not a name binding at all: a method
    /// name resolves to a catalog entry rather than a `SymbolId` (HIR-02). That
    /// case used to be carried as [`SymbolId::UNRESOLVED`], which is a *different*
    /// question — `lower.rs` still uses that constant to mean "resolution ran and
    /// found no declaration". One sentinel answering both left every reader to
    /// guess which it had, so the absence that hover can produce is spelled in
    /// the type instead.
    pub symbol: Option<SymbolId>,
    /// The name as written in source.
    pub name: String,
    /// The inferred type scheme, rendered for display.
    pub scheme: String,
}

impl Analysis {
    /// Hover at `range` (the exact span of a name token). Returns `None` if no
    /// name reference covers that range. The returned [`HoverInfo`] carries the
    /// symbol id and its inferred scheme — so hovering over each shadowed
    /// occurrence of a name returns a distinct `(symbol, type)`.
    #[must_use]
    pub fn hover(&self, range: TextRange) -> Option<HoverInfo> {
        // A method name is not a name reference (HIR-02): it resolves to a
        // catalog entry, not a `SymbolId`, so it has no entry in `refs` and
        // hover used to return nothing at all for `v.len()`'s `len`. Its result
        // type used to be smuggled into `ref_types` at the same range, where the
        // `refs` lookup above meant nothing could ever read it.
        if let Some(m) = self.method_refs.get(&range) {
            return Some(HoverInfo {
                symbol: None,
                name: format!(
                    "{}.{}",
                    self.db.render(self.db.follow(m.receiver)),
                    m.entry.name
                ),
                scheme: self.db.render(self.db.follow(m.result)),
            });
        }
        let resolved = self.refs.get(&range)?;
        let symbol = self.names.get(resolved.symbol)?;
        let ty = self.ref_types.get(&range);
        let scheme = match (ty, &symbol.scheme) {
            // Prefer the per-reference instantiated type when available (it is
            // the concrete type at this use site), else the binding's scheme.
            (Some(t), _) => self.db.render(*t),
            (None, Some(s)) => self.db.render_scheme(s),
            (None, None) => "?".to_string(),
        };
        Some(HoverInfo {
            symbol: Some(resolved.symbol),
            name: symbol.name.clone(),
            scheme,
        })
    }

    /// Hover over a *declaration* site (a `var`/`fn`/param binding),
    /// identified by its name-token range. Uses the `decls`... actually the
    /// decls map is not carried into Analysis; instead, look the symbol up
    /// directly by id among the symbols whose declaration span covers `range`.
    #[must_use]
    pub fn hover_decl(&self, range: TextRange) -> Option<HoverInfo> {
        // Find the symbol declared at this range by matching its decl span.
        let span = range_to_span(range);
        let sym = self
            .names
            .all()
            .iter()
            .find(|s| s.decl.map(|d| d.span == span).unwrap_or(false))?;
        let scheme = sym
            .scheme
            .as_ref()
            .map(|sc| self.db.render_scheme(sc))
            .unwrap_or_else(|| "?".to_string());
        Some(HoverInfo {
            symbol: Some(sym.id),
            name: sym.name.clone(),
            scheme,
        })
    }

    /// Hover over a **parser expression** (§15.3, §19.11 criterion 3).
    ///
    /// Answers the synthesized type of the *innermost* parser node containing
    /// `offset` — so hovering `lines(…)` inside `sections(lines(…))` reports
    /// `Vec[{ … }]` and not the root's `Vec[Vec[{ … }]]`. That distinction is
    /// the whole of ADR-098: before the index existed, the only type in reach
    /// was the root's, and an implementation that answered it everywhere passed
    /// "hover over a parser expression shows a type".
    ///
    /// Returns `None` outside every `read`/`parse` body, and for a body the
    /// compiler rejected — there is no synthesized type for a tree that did not
    /// convert, and inventing one would report about a program that does not
    /// exist.
    #[must_use]
    pub fn hover_parser(&self, offset: u32) -> Option<ParserHoverInfo> {
        let index = self
            .parser_exprs
            .iter()
            .filter(|idx| idx.contains(offset))
            .min_by_key(|idx| u32::from(idx.expr_range.len()))?;
        let ty = index.type_at(offset)?;
        Some(ParserHoverInfo {
            rendered: self.db.render(self.db.follow(ty)),
            mode: index.mode_at(offset),
            is_root: index
                .type_at(offset)
                .zip(index.type_at(u32::from(index.expr_range.start())))
                .is_some_and(|(a, b)| a == b),
        })
    }
}

/// What hover over a parser expression reveals.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParserHoverInfo {
    /// The synthesized result type, rendered by the same `db.render` the CLI
    /// prints — so the editor and `praxis check` name a type the same way.
    pub rendered: String,
    /// Where in the sublanguage the cursor is (§15.3's five-way question).
    pub mode: crate::ParserMode,
    /// Whether this is the whole `read`/`parse` body's type rather than an
    /// inner node's. Lets the presentation say "read result" for the root and
    /// name the node otherwise.
    pub is_root: bool,
}
