//! Rich parse-failure detail (§7.11).
//!
//! A parse mismatch (§7.11) is signalled as [`crate::FaultKind::ParseFailed`],
//! but the fault kind alone carries no detail. The crash debugger and the
//! noninteractive fallback need the *structured* information §7.11 lists:
//!
//! ```text
//! input span          — where in the input the mismatch occurred
//! parser span         — the source span of the failing parser expression
//! expected description — what the parser expected (e.g. "int", "literal ':'")
//! actual preview      — a bounded slice of the input around the mismatch
//! parser path         — the active input parser path (reserved)
//! partial root value  — best-effort deepest successfully-built sub-value
//! ```
//!
//! This module owns those fields. The [`ParseDetail`] lives on the [`Runtime`]
//! (its address is installed on every [`crate::RuntimeContext`] at
//! `parse_detail`), and the parser interpreter writes a [`ParseFail`] into it
//! on every mismatch. The deepest (most specific) failure wins: an inner
//! capture failure (`expected "int"` at offset 12) is more useful than the
//! outer constructor's generic failure, so we keep the failure with the
//! furthest input offset seen.
//!
//! The detail is **host-managed**: generated code never reads or writes it. It
//! is appended at the end of `RuntimeContext` so existing field offsets read by
//! JIT code are unchanged (§11.6 ABI stability).

use crate::gc::GcRef;

/// A single parse mismatch (§7.11), carrying structured detail for the crash
/// debugger and the noninteractive fallback.
///
/// Constructed by the parser interpreter at each failure site via
/// [`ParseFail::here`]; the [`ParseDetail`] slot keeps the most specific one.
#[derive(Clone, Debug)]
pub struct ParseFail {
    /// `[start, end)` byte offsets into the input buffer where the mismatch
    /// occurred. `end` may equal `start` for a zero-width expectation (e.g.
    /// "expected a digit, found end-of-input").
    pub input_span: (usize, usize),
    /// What the parser expected, as a short human description (`"int"`,
    /// `"literal ':'"`, `"section header"`, ``"6 sections for `shapes`"``, …).
    /// A `String` rather than a `&'static str` precisely so a description can
    /// name the thing that came up short.
    pub expected: String,
    /// The source span of the failing parser expression (byte offsets into the
    /// program source). `None` when no span was threaded (an internal-only
    /// plan node); the renderer falls back to `<unknown parser>`.
    pub parser_span: Option<(u32, u32)>,
    /// The best-effort partial root value built before the failure (the deepest
    /// successfully-assembled sub-value), or `None` when no partial value was
    /// available. The collector retains it because the runtime roots
    /// [`ParseDetail`] (see [`crate::ParseDetail`]).
    pub partial: Option<GcRef>,
}

impl ParseFail {
    /// Construct a failure at `offset` (zero-width) expecting `expected`.
    /// Convenience for the common "found X, expected Y at this point" case.
    pub fn here(offset: usize, expected: impl Into<String>) -> Self {
        ParseFail {
            input_span: (offset, offset),
            expected: expected.into(),
            parser_span: None,
            partial: None,
        }
    }

    /// With a width: the mismatch span is `[offset, offset + len)`.
    pub fn at(offset: usize, len: usize, expected: impl Into<String>) -> Self {
        ParseFail {
            input_span: (offset, offset + len),
            expected: expected.into(),
            parser_span: None,
            partial: None,
        }
    }

    /// Attach the source span of the failing parser expression.
    #[must_use]
    pub fn with_parser_span(mut self, span: Option<(u32, u32)>) -> Self {
        self.parser_span = span;
        self
    }

    /// Attach the best-effort partial root value.
    #[must_use]
    pub fn with_partial(mut self, partial: Option<GcRef>) -> Self {
        self.partial = partial;
        self
    }
}

/// The runtime-owned slot that holds the richest parse failure seen during the
/// last `run_plan`, plus a bounded preview of the input around it.
///
/// Lives on [`crate::Runtime`] (so its address is stable) and is exposed to
/// generated code only as an opaque pointer field on
/// [`crate::RuntimeContext`]; the host (CLI / debugger) reads it after a
/// `FaultKind::ParseFailed`.
#[derive(Debug, Default)]
pub struct ParseDetail {
    /// The richest failure, when one occurred this run.
    pub fail: Option<ParseFail>,
    /// A bounded UTF-8-lossy preview of the input bytes around the failure
    /// offset (set by the runtime when the failure is recorded, so the debugger
    /// does not need to re-read the input buffer). Empty until populated.
    pub actual_preview: String,
}

impl ParseDetail {
    /// A fresh, clear detail slot.
    pub fn new() -> Self {
        ParseDetail::default()
    }

    /// Clear any recorded failure. Called at the start of each `run_plan` so a
    /// stale detail from a prior parse does not leak into the next.
    pub fn clear(&mut self) {
        self.fail = None;
        self.actual_preview.clear();
    }

    /// True iff a parse failure was recorded.
    pub fn is_set(&self) -> bool {
        self.fail.is_some()
    }

    /// Consider recording `fail`. The **deepest** failure wins: we keep the one
    /// whose input offset is furthest into the buffer, since that is the most
    /// specific point at which parsing actually broke. Ties keep the first
    /// (earliest-recorded) failure, which is the innermost in source order.
    pub fn consider(&mut self, fail: ParseFail, input: &[u8]) {
        let wins = match &self.fail {
            None => true,
            // Strictly greater: a tie (same offset) keeps the existing failure,
            // which is the innermost in recording order.
            Some(existing) => fail.input_span.0 > existing.input_span.0,
        };
        if wins {
            self.actual_preview = preview_around(input, fail.input_span.0);
            self.fail = Some(fail);
        }
    }
}

/// The number of bytes of context shown on each side of the failure offset in
/// the [`ParseDetail::actual_preview`]. Kept small so the preview is a useful
/// one-line glance for the debugger, not a buffer dump.
const PREVIEW_RADIUS: usize = 24;

/// Build a bounded, UTF-8-lossy, single-line preview of `input` around `offset`.
/// Newlines are rendered as `⏎` so the preview stays on one line.
fn preview_around(input: &[u8], offset: usize) -> String {
    // `start` is clamped to `end`, not merely to the radius. An offset past the
    // end of the buffer gives `start > end`, and `&input[start..end]` panics on
    // an inverted range — inside `extern "C"`, where the ABI guard turns a panic
    // into an internal fault rather than the preview a total function gives
    // (ADR-080). Such an offset is reachable: a ragged grid's fill is parsed
    // against its own buffer, so a failure there carries an offset that means
    // nothing here.
    let end = (offset + PREVIEW_RADIUS).min(input.len());
    let start = offset.saturating_sub(PREVIEW_RADIUS).min(end);
    let slice = &input[start..end];
    let lossy = String::from_utf8_lossy(slice);
    let mut out = String::with_capacity(lossy.len());
    for ch in lossy.chars() {
        if ch == '\n' || ch == '\r' {
            out.push('⏎');
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A failure offset past the end of the buffer must not make the preview
    /// slice an inverted range: `start = offset - 24` and
    /// `end = min(offset + 24, len)`, so without the clamp of `start` to `end`
    /// any offset more than 24 bytes past the end panics in `&input[start..end]`.
    #[test]
    fn a_failure_offset_past_the_buffer_previews_rather_than_panicking() {
        let mut d = ParseDetail::new();
        d.consider(ParseFail::here(10_000, "int"), b"short");
        assert!(d.is_set());
        assert_eq!(
            d.actual_preview, "",
            "there is nothing within 24 bytes of an offset past the end, and nothing is a preview"
        );
    }

    #[test]
    fn first_failure_sets_detail() {
        let mut d = ParseDetail::new();
        d.consider(ParseFail::here(5, "int"), b"abc123");
        assert!(d.is_set());
        assert_eq!(d.fail.as_ref().unwrap().input_span, (5, 5));
        assert_eq!(d.fail.as_ref().unwrap().expected, "int");
    }

    #[test]
    fn deeper_failure_wins() {
        let mut d = ParseDetail::new();
        d.consider(ParseFail::here(3, "outer"), b"abcdef");
        d.consider(ParseFail::here(10, "inner"), b"abcdef");
        // The offset-10 failure is deeper → it wins.
        assert_eq!(d.fail.as_ref().unwrap().expected, "inner");
        assert_eq!(d.fail.as_ref().unwrap().input_span.0, 10);
    }

    #[test]
    fn shallower_failure_loses() {
        let mut d = ParseDetail::new();
        d.consider(ParseFail::here(20, "deep"), b"abcdef");
        d.consider(ParseFail::here(5, "shallow"), b"abcdef");
        // The offset-20 failure stays; the offset-5 one is less specific.
        assert_eq!(d.fail.as_ref().unwrap().expected, "deep");
    }

    #[test]
    fn tie_keeps_first() {
        let mut d = ParseDetail::new();
        d.consider(ParseFail::here(8, "first"), b"abcdef");
        d.consider(ParseFail::here(8, "second"), b"abcdef");
        assert_eq!(d.fail.as_ref().unwrap().expected, "first");
    }

    #[test]
    fn clear_resets() {
        let mut d = ParseDetail::new();
        d.consider(ParseFail::here(3, "int"), b"abc");
        d.clear();
        assert!(!d.is_set());
        assert!(d.actual_preview.is_empty());
    }

    #[test]
    fn preview_is_single_line_and_bounded() {
        let input = b"aaaa\nbbbb\ncccc\ndddd\neeee";
        let preview = preview_around(input, 12);
        assert!(!preview.contains('\n'));
        assert!(!preview.contains('\r'));
        assert!(preview.contains('⏎'));
        // Bounded: at most 2 * PREVIEW_RADIUS characters (plus replacement chars).
        assert!(preview.chars().count() <= 2 * PREVIEW_RADIUS + 4);
    }

    #[test]
    fn preview_at_buffer_start() {
        let preview = preview_around(b"hello world", 0);
        assert!(preview.starts_with("hello"));
    }

    #[test]
    fn preview_at_buffer_end() {
        let preview = preview_around(b"hello world", 11);
        assert!(preview.ends_with("world"));
    }
}
