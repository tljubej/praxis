//! Completion-data generation from the method catalog (§19.8).
//!
//! The §19.8 acceptance criterion requires that "method completion data is
//! generated from the same catalog used by the compiler." This module renders
//! the [`MethodCatalog`](crate::MethodCatalog) into a serializable completion
//! table for completion and signature help (§5.7: "The language server uses the
//! same table"). No LSP wiring here — just the generation, plus a round-trip
//! test proving the generated data covers the compiler's catalog 1:1.

use crate::{MethodCatalog, MethodEntry};

/// One completion item: the receiver shape, method name, parameter shapes,
/// result shape, and doc — everything the LSP needs to offer a completion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionItem {
    /// The receiver type as a display string, e.g. `Vec[T]` or `Map[K, V]`.
    pub receiver: String,
    /// The method name, e.g. `push`.
    pub name: String,
    /// The parameter type-pattern display strings, in order.
    pub params: Vec<String>,
    /// The result type-pattern display string.
    pub result: String,
    /// The one-line doc.
    pub doc: String,
}

/// Generate the full completion table from the catalog, in catalog order.
/// Every entry becomes one [`CompletionItem`]; the output is a 1:1 rendering.
#[must_use]
pub fn completion_data(catalog: &MethodCatalog) -> Vec<CompletionItem> {
    catalog.entries().iter().map(entry_to_item).collect()
}

/// Render one catalog entry as a completion item.
fn entry_to_item(e: &MethodEntry) -> CompletionItem {
    CompletionItem {
        receiver: e.receiver.to_string(),
        name: e.name.to_string(),
        params: e.params.iter().map(|p| p.to_string()).collect(),
        result: e.result.to_string(),
        doc: e.doc.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_data_covers_every_catalog_entry() {
        // The §19.8 acceptance criterion: completion data is generated from the
        // same catalog the compiler uses. Every builtin_catalog() entry must
        // appear in the generated completion data, 1:1.
        let cat = crate::builtin_catalog();
        let items = completion_data(&cat);
        assert_eq!(
            items.len(),
            cat.len(),
            "completion data must cover every catalog entry"
        );
        // Spot-check a Vec and a Map entry (the headline receiver shapes).
        let has_vec_push = items
            .iter()
            .any(|i| i.receiver == "Vec[T]" && i.name == "push");
        assert!(has_vec_push, "Vec[T].push must be in completion data");
        let has_map_insert = items
            .iter()
            .any(|i| i.receiver == "Map[K, V]" && i.name == "insert");
        assert!(
            has_map_insert,
            "Map[K, V].insert must be in completion data"
        );
        let has_grid_neighbors4 = items
            .iter()
            .any(|i| i.receiver == "Grid[T]" && i.name == "neighbors4");
        assert!(
            has_grid_neighbors4,
            "Grid[T].neighbors4 must be in completion data"
        );
    }

    #[test]
    fn completion_data_round_trips_receiver_name_arity() {
        // Every (receiver, name, arity) triple in the catalog is unique (the
        // builder rejects duplicates), so the completion items' triples must
        // also be unique — a 1:1 mapping with no loss.
        let cat = crate::builtin_catalog();
        let items = completion_data(&cat);
        let triples: Vec<(String, String, usize)> = items
            .iter()
            .map(|i| (i.receiver.clone(), i.name.clone(), i.params.len()))
            .collect();
        let unique: std::collections::HashSet<_> = triples.iter().collect();
        assert_eq!(
            unique.len(),
            items.len(),
            "completion triples must be unique"
        );
    }
}
