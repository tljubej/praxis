//! Hover query: given a source position, return the inferred type and symbol
//! identity of the name there (§15.2 hover, §19-M2 criterion 5).
//!
//! The real LSP lands in M11; for M2 this is a library-level query exercised by
//! a test, satisfying "hover returns the inferred type and symbol identity for
//! each shadowed occurrence."

use praxis_types::Scheme;
use rowan::TextRange;

use crate::{Analysis, SymbolId};

/// What hover at a position reveals about a name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HoverInfo {
    /// The symbol the name resolves to. Two shadowed bindings with the same
    /// source name have *different* ids — this is how hover distinguishes them.
    pub symbol: SymbolId,
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
                symbol: SymbolId(u32::MAX),
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
            symbol: resolved.symbol,
            name: symbol.name.clone(),
            scheme,
        })
    }

    /// Hover over a *declaration* site (a `let`/`var`/`fn`/param binding),
    /// identified by its name-token range. Uses the `decls`... actually the
    /// decls map is not carried into Analysis; instead, look the symbol up
    /// directly by id among the symbols whose declaration span covers `range`.
    #[must_use]
    pub fn hover_decl(&self, range: TextRange) -> Option<HoverInfo> {
        // Find the symbol declared at this range by matching its decl span.
        let span = praxis_source::Span::new(
            praxis_source::BytePos::from(u32::from(range.start())),
            praxis_source::BytePos::from(u32::from(range.end())),
        );
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
            symbol: sym.id,
            name: sym.name.clone(),
            scheme,
        })
    }

    /// The rendered scheme of a symbol by id (used by tests and, later, the LSP).
    #[must_use]
    pub fn scheme_of(&self, id: SymbolId) -> Option<&Scheme> {
        self.names.get(id).and_then(|s| s.scheme.as_ref())
    }
}
