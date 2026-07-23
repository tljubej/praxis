//! The unified input parser DSL (§7, §14.1).
//!
//! Responsibility (per the design): the parser-expression lexer, the backtick
//! template parser, static validation, compile-time result-type synthesis, and
//! parser-plan construction. The DSL has its own typed AST (§7.9) and must not
//! be lowered immediately into string-splitting calls.
//!
//! **Milestone 0: skeleton.** The first constructors (`lines`, `sections`,
//! `csv`, `ws`, `sep`, `grid`) land in Milestone 6; heterogeneous `sections`,
//! `block`, `choice`, `scan` follow in Milestone 9.

/// Marker documenting that this crate is a deliberate skeleton.
pub const FILLED_AT_MILESTONE: u32 = 6;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skeleton_reports_fill_milestone() {
        assert_eq!(FILLED_AT_MILESTONE, 6);
    }
}
