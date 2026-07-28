//! The unified input parser DSL (§7, §14.1).
//!
//! Responsibility (per the design): the parser-expression typed AST, the backtick
//! template scanner, static validation, compile-time result-type synthesis, and
//! parser-plan construction. The DSL has its own typed AST (§7.9) and is **not**
//! lowered immediately into string-splitting calls.
//!
//! **Milestone 6** fills this crate: atomics (`int`/`char`/`word`/`text`/
//! `rest`/`digit`) and constructors (`lines`/`sections`/`csv`/`ws`/`sep`/
//! `grid`), backtick templates, type synthesis, and plan lowering. The runtime
//! interpreter lives in `praxis-runtime::parser`. Heterogeneous `sections`,
//! `block`, `choice`, `scan` follow in Milestone 9.

pub mod ast;
pub mod plan;
pub mod scan;
pub mod synthesize;
pub mod validate;

pub use ast::{AtomicKind, BlockItem, Constructor, ParserAst, SkipPolicy, TemplatePart, WsPolicy};
pub use plan::{
    get_plan, lower_to_plan, plan_count, register_plan, retire_all_plans, BlockItemNode,
    CompiledPlan, ParserPlan, PlanId, PlanNode, TemplatePartNode, TooManyPlans, MAX_PLANS,
};
pub use scan::{scan_template, ScanError};
pub use synthesize::synthesize;
pub use validate::{check_constructor_arity, validate, ValidationError};

/// Marker documenting that this crate is filled at Milestone 6.
pub const FILLED_AT_MILESTONE: u32 = 6;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn milestone_marker_is_six() {
        assert_eq!(FILLED_AT_MILESTONE, 6);
    }
}
