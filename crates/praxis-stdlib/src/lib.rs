//! The method catalog schema and the Praxis prelude (§16).
//!
//! The single source of truth for what built-in methods exist, what their
//! types are, and how they lower to runtime symbols or intrinsics. Per the
//! design's hard rule (§20, rule 3: "never duplicate type or method knowledge
//! between compiler and LSP"), the type checker, HIR lowering, code generator,
//! documentation generator, and language-server completion all consume *this*
//! catalog. So the schema has to carry everything each of those consumers needs
//! in one place (§16.2).

pub mod abi;
pub mod builtins;
pub mod capability;
pub mod catalog;
pub mod completion;
pub mod prelude;
pub mod type_pattern;

pub use builtins::builtin_catalog;
pub use capability::CapKind;
pub use catalog::{MethodCatalog, MethodCatalogError, MethodEntry, MethodLowering, Purity};
pub use completion::{CompletionItem, completion_data};
pub use prelude::{
    BUILTIN_TYPES, GRAPH_HELPERS, GraphHelper, GraphParam, GraphResult, NUMERIC_HELPERS,
    NumericHelper, PRELUDE, SIZED_CTORS, SizedCtor, TypeEntry, graph_helper, numeric_helper,
    prelude_doc, sized_ctor, type_doc,
};
pub use type_pattern::{
    Bound, CollectionCtor, PIPELINE_RECEIVERS, TypePattern, is_pipeline_receiver, pattern_matches,
};
