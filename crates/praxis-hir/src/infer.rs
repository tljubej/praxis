//! Type inference (Slice 5).
//!
//! Placeholder for Slice 4: passes the name resolution through unchanged,
//! producing an empty type arena and no `Y0xx` diagnostics. Slice 5 fills in
//! the real inference engine that walks the resolved tree and assigns a
//! [`Scheme`] to every binding and a [`Type`] to every expression.

use praxis_source::FileId;

use crate::resolve::NameResolution;
use praxis_types::TypeDb;

/// The output of inference: the type arena plus the (possibly extended)
/// resolution. Slice 5 adds `ref_types` and `Y0xx` diagnostics.
pub struct Inference {
    pub db: TypeDb,
    pub names: crate::NameTable,
    pub scopes: crate::ScopeTree,
    pub refs: std::collections::HashMap<rowan::TextRange, crate::ResolvedRef>,
    pub ref_types: std::collections::HashMap<rowan::TextRange, praxis_types::Type>,
    pub diagnostics: Vec<praxis_source::Diagnostic>,
}

/// Slice-4 stub: resolution only, no inference. Slice 5 replaces the body.
pub(crate) fn infer(_file: FileId, resolution: NameResolution) -> Inference {
    Inference {
        db: TypeDb::new(),
        names: resolution.names,
        scopes: resolution.scopes,
        refs: resolution.refs,
        ref_types: std::collections::HashMap::new(),
        diagnostics: resolution.diagnostics,
    }
}
