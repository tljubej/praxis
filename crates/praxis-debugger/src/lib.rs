//! The crash debugger (§9, §14.1) — and the breakpoint one (§9.8).
//!
//! Responsibility (per the design): capture stable per-frame snapshots on
//! fault propagation (function id, current source span, named local slots,
//! local type descriptors, active input parser path), and serve the interactive
//! crash REPL with `bt`, `frame`, `locals`, `p EXPR`, `type EXPR`, `source`,
//! `input`, `parser`, `heap`, `restart`, `reload`, `quit`, `help`.
//!
//! The pieces: snapshot rendering, the noninteractive fallback (§9.6), the
//! interactive REPL's navigation and locals (`bt`/`frame`/`up`/`down`/
//! `locals`), the read-only `p EXPR`/`type EXPR` evaluator, and the `source`/
//! `input`/`parser`/`heap` context commands with `restart`/`reload`. The
//! [`session`] module owns the live compile/run state those commands reach.
//!
//! ## The second way in
//!
//! A `:bp` marker (§9.8, ADR-150) stops a program that has **not** faulted, and
//! the same [`repl`] and [`tui`] serve it: the snapshot is the same deep copy of
//! the same debug chain, so the questions and their answers are the same. What
//! differs is which commands the situation supports, and
//! [`repl::Repl`]'s attachment settles that in one place — a stopped program has
//! frames to return to, so it gains `continue`; it is in the middle of using its
//! own runtime, so it loses everything that would execute.

pub mod evaluate;
pub mod purity;
pub mod render;
pub mod repl;
pub mod session;
pub mod synth;
pub mod tui;
pub mod value;

/// The milestone this crate's implementation belongs to.
pub const FILLED_AT_MILESTONE: u32 = 10;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skeleton_reports_fill_milestone() {
        assert_eq!(FILLED_AT_MILESTONE, 10);
    }
}
