//! Property-test fuzz gate for the lexer and parser (ADR-006, §19 acceptance).
//!
//! Feeds arbitrary input to [`praxis_parser::lex`] and [`praxis_parser::parse`]
//! and asserts they **terminate without panicking**. This is the Milestone 1
//! acceptance criterion "no panic on fuzzed token streams." The parser is
//! additionally expected to always return a (possibly error-filled) tree rather
//! than abort.
//!
//! **Generator coverage.** `lex`/`parse` take `&str`, so invalid UTF-8 cannot
//! reach them: the byte-level boundary is the caller's file read, not this API.
//! What the generators must cover is therefore the full *scalar* space, and the
//! earlier `".{0,256}"` did not — a regex `.` excludes `\n`, so every
//! newline-sensitive path (line comments, statement layout) went unfuzzed, and
//! purely random scalars almost never form a token the parser recognizes. The
//! generators below use `(?s)` for the any-scalar case and add an alphabet
//! weighted toward real Praxis syntax so the parser is exercised past its first
//! error.
//!
//! Runs on stable Rust via `proptest`; `cargo-fuzz` can supplement this later
//! with coverage-guided fuzzing.

use praxis_parser::{lex, parse};
use proptest::prelude::*;

/// Any scalar sequence, newlines included.
fn any_text() -> impl Strategy<Value = String> {
    "(?s).{0,256}"
}

/// Input drawn from a Praxis-shaped alphabet: keywords, operators, layout, and
/// non-ASCII scalars on both sides of the identifier boundary.
fn praxis_shaped_text() -> impl Strategy<Value = String> {
    let piece = prop_oneof![
        Just("let "),
        Just("var "),
        Just("fn f("),
        Just("if "),
        Just("for x in "),
        Just("match "),
        Just("return"),
        Just("read "),
        Just("=> "),
        Just("{"),
        Just("}"),
        Just("("),
        Just(")"),
        Just("["),
        Just("]"),
        Just("."),
        Just("..="),
        Just(","),
        Just(";"),
        Just("\n"),
        Just("  "),
        Just("// comment\n"),
        Just("/* nested /* inner */ */"),
        Just("\"text\\n\""),
        Just("`{x:int}`"),
        Just("42"),
        Just("3.14"),
        Just("_"),
        // Non-ASCII identifiers and non-identifier scalars, which the old byte
        // classifier treated identically.
        Just("λ"),
        Just("日本語"),
        Just("e\u{0301}"),
        Just("→"),
        Just("🦀"),
    ];
    prop::collection::vec(piece, 0..48).prop_map(|parts| parts.concat())
}

proptest! {
    // Keep the case count and input size modest: this runs on every CI build,
    // and the point is termination/panic-freedom, not exhaustive coverage.
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Lexing arbitrary input must never panic.
    #[test]
    fn lex_never_panics(input in any_text()) {
        let _ = lex(praxis_source::FileId::SYNTHETIC, &input);
    }

    /// Parsing arbitrary input must never panic and must always produce a tree.
    #[test]
    fn parse_never_panics_and_returns_tree(input in any_text()) {
        let out = parse(praxis_source::FileId::SYNTHETIC, &input);
        // The root is always a SOURCE_FILE, even for total garbage.
        prop_assert_eq!(out.tree.kind(), praxis_syntax::SyntaxKind::SOURCE_FILE);
    }

    /// The same, on input shaped like real Praxis so the parser gets past its
    /// first error and into recovery.
    #[test]
    fn parse_never_panics_on_praxis_shaped_input(input in praxis_shaped_text()) {
        let out = parse(praxis_source::FileId::SYNTHETIC, &input);
        prop_assert_eq!(out.tree.kind(), praxis_syntax::SyntaxKind::SOURCE_FILE);
    }

    /// Token spans must exactly tile the input: start at 0, be contiguous, end
    /// at the input length, and never split a scalar. This is the invariant a
    /// byte-at-a-time advance over multi-byte input violates.
    #[test]
    fn token_spans_tile_the_input(input in praxis_shaped_text()) {
        let out = lex(praxis_source::FileId::SYNTHETIC, &input);
        let mut at = 0usize;
        for token in &out.tokens {
            let (start, end) = (token.span.start().to_usize(), token.span.end().to_usize());
            prop_assert_eq!(start, at, "gap or overlap before {:?}", token.kind);
            prop_assert!(end >= start);
            prop_assert!(
                input.is_char_boundary(start) && input.is_char_boundary(end),
                "token {:?} splits a scalar", token.kind
            );
            at = end;
        }
        prop_assert_eq!(at, input.len(), "tokens do not cover the input");
    }

    /// The lossless tree must reproduce the source byte-for-byte (ADR-003).
    #[test]
    fn the_tree_round_trips_the_source(input in praxis_shaped_text()) {
        let out = parse(praxis_source::FileId::SYNTHETIC, &input);
        prop_assert_eq!(out.tree.text().to_string(), input);
    }
}
