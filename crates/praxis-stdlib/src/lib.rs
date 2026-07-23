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

pub mod catalog;
pub mod prelude;
pub mod type_pattern;

pub use catalog::{
    MethodCatalog, MethodCatalogError, MethodEntry, MethodLowering, Purity, Stability,
};
pub use prelude::PRELUDE;
pub use type_pattern::TypePattern;
