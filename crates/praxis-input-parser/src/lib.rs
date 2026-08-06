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
pub mod body;
pub mod call;
pub mod plan;
pub mod scan;
pub mod synthesize;
pub mod validate;

pub use ast::{
    ArgShape, AtomicKind, BlockItem, CaptureName, Constructor, EmptySeparator, InvalidCaptureName,
    InvalidRepeatCount, ParserAst, RepeatCount, SectionItem, Separator, SkipPolicy, TemplatePart,
    WsPolicy,
};
pub use call::{build_call, build_repeated_tail, CallArg};
pub use plan::{
    get_plan, lower_to_plan, plan_count, register_plan, retire_all_plans, BlockItemNode,
    CompiledPlan, ParserPlan, PlanId, PlanNode, SectionItemNode, TemplatePartNode, TemplateShape,
    TooManyPlans, MAX_PLANS,
};
pub use scan::{scan_template, ScanError, MAX_NESTING};
pub use synthesize::{synthesize, synthesize_indexed};
pub use validate::{check_call, validate, ArgKind, ValidationError};

/// Marker documenting that this crate is filled at Milestone 6.
pub const FILLED_AT_MILESTONE: u32 = 6;

/// Every name a parser expression may begin with: §7.4's atomics and §7.5's
/// constructors, in their own tables' order.
///
/// The two are one list because they are one thing to a *user* — the word after
/// `read` or inside `{…}` — and the two diagnostics for getting it wrong
/// (`I010`, `I013`) are the same mistake seen from two tables. A "did you mean"
/// that only knew one of them would answer `int` for `intt` and nothing for
/// `line`.
pub fn parser_names() -> impl Iterator<Item = &'static str> {
    AtomicKind::ALL
        .iter()
        .map(|a| a.keyword())
        .chain(Constructor::ALL.iter().map(|c| c.keyword()))
}

/// The atomic or constructor `name` was probably meant to be (ADR-132).
///
/// §15.3's own example: `line` answers `lines`. The threshold is
/// [`praxis_source::nearest`]'s, shared with every other did-you-mean in the
/// compiler, so one place decides when a near miss is near enough.
#[must_use]
pub fn nearest_parser_name(name: &str) -> Option<&'static str> {
    praxis_source::nearest(name, parser_names())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn milestone_marker_is_six() {
        assert_eq!(FILLED_AT_MILESTONE, 6);
    }
}
