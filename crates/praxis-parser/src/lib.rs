//! Lexer and parser for the Praxis language.
//!
//! - [`lex`] turns source text into a lossless token stream (including trivia)
//!   plus `T0xx` diagnostics.
//! - [`parse`] runs the lexer and then a recursive-descent + Pratt parser
//!   (ADR-004) over the M1 grammar, producing a rowan-backed lossless tree
//!   (ADR-003) plus `P0xx` diagnostics. The tree retains trivia so the
//!   formatter, LSP, and code actions can use it (§13.1).

pub mod fmt;
pub mod lex;
pub mod parse;

pub use fmt::{format_node, format_source};
pub use lex::{lex, LexOutput};
pub use parse::{parse, ParseOutput, TYPE_CONSTRUCTOR_NAMES};
