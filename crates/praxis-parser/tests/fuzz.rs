//! Property-test fuzz gate for the lexer and parser (ADR-006, §19 acceptance).
//!
//! Feeds arbitrary byte strings to [`praxis_parser::lex`] and
//! [`praxis_parser::parse`] and asserts only that they **terminate without
//! panicking**. This is the Milestone 1 acceptance criterion "no panic on
//! fuzzed token streams." The parser is additionally expected to always return
//! a (possibly error-filled) tree rather than abort.
//!
//! Runs on stable Rust via `proptest`; `cargo-fuzz` can supplement this later
//! with coverage-guided fuzzing.

use praxis_parser::{lex, parse};
use proptest::prelude::*;

proptest! {
    // Keep the case count and input size modest: this runs on every CI build,
    // and the point is termination/panic-freedom, not exhaustive coverage.
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Lexing arbitrary input must never panic.
    #[test]
    fn lex_never_panics(input in ".{0,256}") {
        let _ = lex(praxis_source::FileId::SYNTHETIC, &input);
    }

    /// Parsing arbitrary input must never panic and must always produce a tree.
    #[test]
    fn parse_never_panics_and_returns_tree(input in ".{0,256}") {
        let out = parse(praxis_source::FileId::SYNTHETIC, &input);
        // The root is always a SOURCE_FILE, even for total garbage.
        prop_assert_eq!(out.tree.kind(), praxis_syntax::SyntaxKind::SOURCE_FILE);
    }
}
