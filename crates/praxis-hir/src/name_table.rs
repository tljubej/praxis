//! The symbol table: the interned set of all [`Symbol`]s, indexed by [`SymbolId`].
//!
//! Insertion mints a fresh, monotonically-increasing id and writes it back into
//! the symbol, so a caller never has to guess an id. This keeps the "two
//! shadowed bindings have distinct ids" invariant structural (AGENTS.md):
//! `insert` always allocates a new slot, never reuses one.

use std::collections::HashMap;

use crate::symbol::{Symbol, SymbolId};

/// All symbols in one compilation unit, addressable by id and by name+scope via
/// the [`ScopeTree`](crate::scope::ScopeTree). The table owns the symbols; the
/// scope tree owns the name→id mapping.
#[derive(Clone, Debug, Default)]
pub struct NameTable {
    symbols: Vec<Symbol>,
    /// Quick lookup by id is the common path (index into `symbols`); this map is
    /// kept for future reverse queries (e.g. "all symbols named `a`").
    #[allow(dead_code)]
    by_name: HashMap<String, Vec<SymbolId>>,
}

impl NameTable {
    /// Insert `sym` (its `id` field is overwritten with the freshly-minted id)
    /// and return that id.
    pub fn insert(&mut self, mut sym: Symbol) -> SymbolId {
        let id = SymbolId(self.symbols.len() as u32);
        sym.id = id;
        self.by_name.entry(sym.name.clone()).or_default().push(id);
        self.symbols.push(sym);
        id
    }

    /// Look up a symbol by id.
    #[must_use]
    pub fn get(&self, id: SymbolId) -> Option<&Symbol> {
        self.symbols.get(id.0 as usize)
    }

    /// Mutably look up a symbol by id (used by inference to fill in `scheme`).
    pub fn get_mut(&mut self, id: SymbolId) -> Option<&mut Symbol> {
        self.symbols.get_mut(id.0 as usize)
    }

    /// All symbols, in insertion order.
    #[must_use]
    pub fn all(&self) -> &[Symbol] {
        &self.symbols
    }

    /// Number of symbols.
    #[must_use]
    pub fn len(&self) -> usize {
        self.symbols.len()
    }

    /// Whether there are no symbols.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.symbols.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbol::SymbolKind;

    #[test]
    fn insert_assigns_distinct_increasing_ids() {
        let mut table = NameTable::default();
        let a = table.insert(Symbol {
            id: SymbolId(0),
            name: "a".into(),
            kind: SymbolKind::Let,
            decl: None,
            scheme: None,
        });
        let b = table.insert(Symbol {
            id: SymbolId(0),
            name: "a".into(), // same name, shadowing
            kind: SymbolKind::Let,
            decl: None,
            scheme: None,
        });
        assert_ne!(a, b, "shadowed bindings must have distinct ids");
        assert_eq!(a, SymbolId(0));
        assert_eq!(b, SymbolId(1));
        // The id field was written back.
        assert_eq!(table.get(a).unwrap().id, a);
        assert_eq!(table.get(b).unwrap().id, b);
        assert_eq!(table.get(a).unwrap().name, "a");
    }
}
