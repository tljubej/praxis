//! Lexer and parser for the Praxis language.
//!
//! Per the milestone plan (§19), the full lossless lexer and parser land in
//! Milestone 1. This crate currently exposes a deliberately small **lexer
//! stub**: enough to walk a `.px` file, classify the common token kinds, and
//! emit a real [`Diagnostic`](praxis_source::Diagnostic) for genuinely
//! unexpected bytes. The stub proves the `tokens + diagnostics` pipeline end to
//! end and gives the CLI something real to run for Milestone 0.

pub mod lex;

pub use lex::{lex, LexOutput};
