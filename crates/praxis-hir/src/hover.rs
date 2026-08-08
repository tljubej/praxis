//! Hover query: given a source position, return the inferred type and symbol
//! identity of the name there (§15.2 hover).
//!
//! A library-level query: the language server presents what it answers, and
//! each shadowed occurrence of a name answers its own `(symbol, type)`.

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
    /// name resolves to a catalog entry rather than a `SymbolId`. That absence
    /// is spelled in the type and not as [`SymbolId::UNRESOLVED`], which is a
    /// *different* question — `lower.rs` uses that constant to mean "resolution
    /// ran and found no declaration", and one sentinel answering both would
    /// leave every reader to guess which it had.
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
        // A method name is not a name reference: it resolves to a catalog
        // entry, not a `SymbolId`, so it has no entry in `refs` and must be
        // answered from `method_refs` before the `refs` lookup below.
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

    /// Hover over a *declaration* site (a `var`/`fn`/param binding), identified
    /// by its name-token range: the answer is the symbol whose own recorded
    /// declaration span is exactly `range`.
    ///
    /// A declaration is not a reference, so it has no entry in `refs` and
    /// [`Analysis::hover`] cannot answer it.
    #[must_use]
    pub fn hover_decl(&self, range: TextRange) -> Option<HoverInfo> {
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
    /// the whole of ADR-098, and it is what the per-node index exists for:
    /// answering the root's type everywhere would also "show a type".
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
