//! Lexer and parser for the Praxis language.
//!
//! - [`lex`] turns source text into a lossless token stream (including trivia)
//!   plus `T0xx` diagnostics.
//! - [`parse`] runs the lexer and then a recursive-descent + Pratt parser
//!   (ADR-004) over the grammar, producing a rowan-backed lossless tree
//!   (ADR-003) plus `P0xx` diagnostics. The tree retains trivia, so the LSP and
//!   its code actions can rewrite a span without disturbing the comments and
//!   whitespace around it.

pub mod lex;
pub mod parse;

pub use lex::{LexOutput, lex};
pub use parse::{ParseOutput, TYPE_CONSTRUCTOR_NAMES, parse};
