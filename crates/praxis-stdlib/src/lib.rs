//! The method catalog schema and the Praxis prelude (§16).
//!
//! The single source of truth for what built-in methods exist, what their
//! types are, and how they lower to runtime symbols or intrinsics. Per the
//! design's hard rule (§20, rule 3: "never duplicate type or method knowledge
//! between compiler and LSP"), the type checker, HIR lowering, code generator,
//! documentation generator, and language-server completion all consume *this*
//! catalog. So the schema has to carry everything each of those consumers needs
//! in one place (§16.2).
//!
//! Milestone 0 ships the schema plus seed entries that prove lookups and the
//! "reject duplicate entries" invariant. The full method set is filled in
//! alongside the features that exercise each method.

pub mod abi;
pub mod builtins;
pub mod capability;
pub mod catalog;
pub mod completion;
pub mod prelude;
pub mod type_pattern;

pub use builtins::builtin_catalog;
pub use capability::CapKind;
pub use catalog::{
    MethodCatalog, MethodCatalogError, MethodEntry, MethodLowering, Purity, Stability,
};
pub use completion::{completion_data, CompletionItem};
pub use prelude::{
    graph_helper, numeric_helper, prelude_doc, sized_ctor, type_doc, GraphHelper, GraphParam,
    GraphResult, NumericHelper, SizedCtor, TypeEntry, BUILTIN_TYPES, GRAPH_HELPERS,
    NUMERIC_HELPERS, PRELUDE, SIZED_CTORS,
};
pub use type_pattern::{
    is_pipeline_receiver, pattern_matches, Bound, CollectionCtor, TypePattern, PIPELINE_RECEIVERS,
};
