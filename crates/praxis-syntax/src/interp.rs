//! Where a `"…"` literal ends and where its holes are — **one** answer, for
//! both readers (§8.1, ADR-147).
//!
//! Since ADR-147 a `{` inside a text literal opens an *interpolation hole*
//! holding a full expression, so `"Part 2: {part2}"` is no longer one token: the
//! lexer splits it into fragment tokens with the hole's ordinary tokens between
//! them, which is what gives a name inside a hole a real range in the lossless
//! tree and therefore makes it a closure capture (ADR-147 decision 1).
//!
//! Two readers have to agree about the extent of such a run: [`text_end`], which
//! the lexer asks **before** it emits anything, and [`fragment_end`], which the
//! lexer asks again each time a hole closes and it has to resume scanning text.
//! They are the same rule read from two starting points, so they live in one
//! module and call one another — [`template`](crate::template)'s doc records
//! what happened the last time two scanners each owned a copy of one rule.
//!
//! # The rule
//!
//! A text literal ends at the line it opens on; that predates interpolation and
//! is why [`crate::template`] gives backtick templates the same bound. Within a
//! literal:
//!
//! - `\` hides the next scalar, so `"\""` is one literal and `"\{"` is a
//!   literal brace ([`crate::literal::decode_escape`] owns which escapes mean
//!   what; this module only needs to know that a backslash consumes one scalar).
//! - `{` opens a hole. A `}` in ordinary text closes nothing and is literal —
//!   the asymmetry is deliberate, because outside a hole there is nothing for a
//!   `}` to be ambiguous with.
//! - Inside a hole the scanner reads *expression* source: braces nest, and a
//!   `"…"`, a `'…'`, a `` `…` `` and a `/* … */` are each skipped whole, because
//!   each of them can hold a `}` that is not structure. A nested `"…"` is
//!   skipped by re-entering [`text_end`], so `"{f("{y}")}"` is one literal
//!   containing another.
//! - A `//` inside a hole means the rest of the line is a comment, so the
//!   literal cannot close on its line: that is an unterminated literal, and it
//!   is reported as one.
//!
//! # Why an unterminated run is the whole answer
//!
//! [`TextEnd::Unterminated`] is not a detail of error reporting. The lexer only
//! enters interpolation mode — the brace-depth stack that decides whether a `}`
//! closes a hole or a block — for a literal this module has already proved
//! closes. So there is no path on which a newline or an EOF reaches that stack,
//! and an unterminated literal is exactly what it was before ADR-147: one
//! `TextLit` token plus `T004` (ADR-147 decision 5).
//!
//! That is also why nesting past [`MAX_INTERPOLATION_NESTING`] answers
//! `Unterminated` rather than "stop treating quotes as structure". Refusing to
//! *enter* is a bound both readers observe by construction; a bound that changed
//! what a quote means would put the lexer's resume path and this scanner on
//! different rules at exactly the depth nobody tests.

use crate::template::template_end;
use crate::MAX_INTERPOLATION_NESTING;

/// Where a `"…"` literal ends, and whether it has any holes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextEnd {
    /// The literal closed on its own line with every hole balanced.
    Closed {
        /// Just past the closing quote, so `&src[open..end]` is the whole
        /// literal, quotes included.
        end: usize,
        /// The index of the `{` opening the **first** hole, or `None` when the
        /// literal has none.
        ///
        /// `None` is what keeps an ordinary literal on the path it has always
        /// been on: one `TextLit` token, no mode stack, no new node.
        first_hole: Option<usize>,
    },
    /// The line ended, the text ended, a hole never closed, or the nesting bound
    /// was reached. The index is where the scan **stopped**, so
    /// `&src[open..stopped]` is still a bounded token — the same bound ADR-094
    /// gave an unterminated template.
    Unterminated { stopped: usize },
}

/// What ends a run of literal text inside a `"…"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FragmentEnd {
    /// A hole opened. The index is of the `{`, so the fragment token the lexer
    /// emits covers up to and including it.
    Hole(usize),
    /// The literal closed. The index is **just past** the quote.
    Close(usize),
}

/// Find the end of the `"…"` literal whose opening quote is at `open`, and the
/// first hole in it.
///
/// `src[open]` must be `"`.
///
/// This is the question the lexer asks first, and its answer decides which of
/// two token shapes the literal gets: a `Closed` with no `first_hole` is one
/// `TextLit`, a `Closed` with one is a fragment/hole/fragment run, and an
/// `Unterminated` is one `TextLit` plus `T004`.
#[must_use]
pub fn text_end(src: &str, open: usize) -> TextEnd {
    match run(src, open, 1) {
        Ok((end, first_hole)) => TextEnd::Closed { end, first_hole },
        Err(stopped) => TextEnd::Unterminated { stopped },
    }
}

/// Find where the run of literal text starting just past `at` ends.
///
/// `src[at]` is the delimiter the fragment opens on: the `"` that opened the
/// literal, or the `}` that closed the hole before it. `None` means the line or
/// the text ended first.
///
/// The lexer calls this to resume after a hole closes. It is the same scan
/// [`text_end`] performs, entered in the middle — which is the point of having
/// one function: the pre-scan proved the literal closes, and the resume path has
/// to find the very same fragment boundaries inside it.
#[must_use]
pub fn fragment_end(src: &str, at: usize) -> Option<FragmentEnd> {
    debug_assert!(matches!(src.as_bytes().get(at), Some(b'"') | Some(b'}')));
    fragment(src.as_bytes(), at).ok()
}

/// One whole literal. `level` is 1 for the outermost; a literal nested inside a
/// hole of this one is 2, and so on.
///
/// `Ok((end, first_hole))` is just past the closing quote; `Err(stopped)` is
/// where the scan gave up.
fn run(src: &str, open: usize, level: usize) -> Result<(usize, Option<usize>), usize> {
    let bytes = src.as_bytes();
    let mut at = open;
    let mut first_hole = None;
    loop {
        match fragment(bytes, at)? {
            FragmentEnd::Close(end) => return Ok((end, first_hole)),
            FragmentEnd::Hole(brace) => {
                first_hole.get_or_insert(brace);
                // Resume the next fragment *on* the closing brace, which is the
                // delimiter that fragment opens with — the same position the
                // lexer's resume path is at when it reaches it.
                at = hole(src, brace, level)?;
            }
        }
    }
}

/// One run of literal text, from the delimiter at `at` to the next `{` or the
/// closing `"`.
fn fragment(bytes: &[u8], at: usize) -> Result<FragmentEnd, usize> {
    let mut pos = at + 1;
    while pos < bytes.len() {
        match bytes[pos] {
            // A text literal ends at the line it opens on. The `\r` is taken
            // with the `\n` it precedes so no token ends mid-CRLF.
            b'\n' => return Err(pos),
            b'\r' if bytes.get(pos + 1) == Some(&b'\n') => return Err(pos),
            b'\\' => {
                // An escape hides the next scalar but cannot hide a line break:
                // a trailing `\` is a dangling escape, not a continuation.
                if matches!(bytes.get(pos + 1), Some(b'\n') | None)
                    || (bytes.get(pos + 1) == Some(&b'\r') && bytes.get(pos + 2) == Some(&b'\n'))
                {
                    return Err(pos + 1);
                }
                pos = skip_scalar(bytes, pos + 1);
            }
            b'{' => return Ok(FragmentEnd::Hole(pos)),
            b'"' => return Ok(FragmentEnd::Close(pos + 1)),
            _ => pos = skip_scalar(bytes, pos),
        }
    }
    Err(bytes.len())
}

/// One hole, from the `{` at `open` to its matching `}`. Answers the index
/// **of** that `}` — the delimiter the next fragment opens on.
///
/// The body is expression source, so everything in it that can hold a `}` is
/// skipped whole. Getting any one of these wrong does not merely mis-measure the
/// hole: it moves where the literal ends, and the lexer would then tokenize the
/// rest of the line as something the program did not write.
fn hole(src: &str, open: usize, level: usize) -> Result<usize, usize> {
    // Refuse to *enter* past the bound rather than changing what a delimiter
    // means at depth — see the module doc. The caller turns this into an
    // ordinary unterminated literal.
    if level > MAX_INTERPOLATION_NESTING {
        return Err(open);
    }
    let bytes = src.as_bytes();
    let mut pos = open + 1;
    let mut depth = 1usize;
    while pos < bytes.len() {
        match bytes[pos] {
            b'\n' => return Err(pos),
            b'\r' if bytes.get(pos + 1) == Some(&b'\n') => return Err(pos),
            b'{' => {
                depth += 1;
                pos += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(pos);
                }
                pos += 1;
            }
            // A nested text literal, which may itself hold holes: `"{f("{y}")}"`
            // is one literal containing another. Re-entering `run` is what makes
            // the two agree — a scan that merely looked for the next quote would
            // stop inside the inner literal's own hole.
            b'"' => pos = run(src, pos, level + 1)?.0,
            // `'}'` is a character, not the end of the hole (ADR-141).
            b'\'' => pos = char_literal(bytes, pos)?,
            // A backtick template's interior is the input-parser DSL's and is
            // full of braces. `template_end` is the one rule for its extent, and
            // it is the rule the lexer's `eat_template` follows.
            b'`' => match template_end(src, pos) {
                crate::template::TemplateEnd::Closed(end) => pos = end,
                crate::template::TemplateEnd::Unterminated(stopped) => return Err(stopped),
            },
            // `//` eats the rest of the line, so the literal cannot close on it.
            b'/' if bytes.get(pos + 1) == Some(&b'/') => return Err(pos),
            b'/' if bytes.get(pos + 1) == Some(&b'*') => pos = block_comment(bytes, pos)?,
            _ => pos = skip_scalar(bytes, pos),
        }
    }
    Err(bytes.len())
}

/// One `'…'` run, honouring `\`. Answers the index just past the closing quote.
fn char_literal(bytes: &[u8], open: usize) -> Result<usize, usize> {
    let mut pos = open + 1;
    while pos < bytes.len() {
        match bytes[pos] {
            b'\n' => return Err(pos),
            b'\r' if bytes.get(pos + 1) == Some(&b'\n') => return Err(pos),
            b'\\' => {
                if matches!(bytes.get(pos + 1), Some(b'\n') | None) {
                    return Err(pos + 1);
                }
                pos = skip_scalar(bytes, pos + 1);
            }
            b'\'' => return Ok(pos + 1),
            _ => pos = skip_scalar(bytes, pos),
        }
    }
    Err(bytes.len())
}

/// One `/* … */` run, nestable exactly as the lexer's own is. A block comment
/// may span lines and a text literal may not, so a line break inside one ends
/// the literal.
fn block_comment(bytes: &[u8], open: usize) -> Result<usize, usize> {
    let mut pos = open + 2;
    let mut depth = 1usize;
    while pos < bytes.len() {
        if bytes[pos] == b'\n' {
            return Err(pos);
        }
        if bytes[pos..].starts_with(b"/*") {
            depth += 1;
            pos += 2;
        } else if bytes[pos..].starts_with(b"*/") {
            depth -= 1;
            pos += 2;
            if depth == 0 {
                return Ok(pos);
            }
        } else {
            pos = skip_scalar(bytes, pos);
        }
    }
    Err(bytes.len())
}

/// The index just past the whole UTF-8 scalar beginning at `pos`.
fn skip_scalar(bytes: &[u8], pos: usize) -> usize {
    if pos >= bytes.len() {
        return bytes.len();
    }
    let mut next = pos + 1;
    while next < bytes.len() && (bytes[next] & 0xC0) == 0x80 {
        next += 1;
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    fn closed(src: &str) -> bool {
        matches!(text_end(src, 0), TextEnd::Closed { end, .. } if end == src.len())
    }

    fn holes(src: &str) -> Option<usize> {
        match text_end(src, 0) {
            TextEnd::Closed { first_hole, .. } => first_hole,
            TextEnd::Unterminated { .. } => panic!("expected a closed literal: {src}"),
        }
    }

    /// A literal with no brace in it is exactly what it was before ADR-147, and
    /// says so: `first_hole` is `None`, which is the answer that keeps it a
    /// single `TextLit`.
    #[test]
    fn a_literal_with_no_brace_has_no_holes() {
        for src in [r#""""#, r#""hello""#, r#""a\"b""#, r#""tab\there""#] {
            assert!(closed(src), "{src}");
            assert_eq!(holes(src), None, "{src}");
        }
    }

    /// A `}` in ordinary text closes nothing (ADR-147 decision 4). This is the
    /// asymmetry the decision records, and the case that would break if the
    /// scanner tracked depth in text as well as in holes.
    #[test]
    fn a_brace_that_closes_nothing_is_literal_text() {
        assert!(closed(r#""}""#));
        assert_eq!(holes(r#""}""#), None);
        assert!(closed(r#""a } b""#));
        assert_eq!(holes(r#""a } b""#), None);
    }

    #[test]
    fn a_hole_is_found_at_its_opening_brace() {
        assert_eq!(holes(r#""Part 2: {p}""#), Some(9));
        assert_eq!(holes(r#""{p}""#), Some(1));
        // Only the *first* hole is reported; the rest are found by the resume
        // path, which is `fragment_end`.
        assert_eq!(holes(r#""{a}{b}""#), Some(1));
    }

    /// An escaped brace opens nothing, so `"\{"` is a literal brace and the
    /// literal has no holes at all.
    #[test]
    fn an_escaped_brace_opens_no_hole() {
        assert!(closed(r#""\{""#));
        assert_eq!(holes(r#""\{""#), None);
        assert_eq!(holes(r#""\{not a hole\}""#), None);
        // …and an escape before the brace does not hide the *next* one.
        assert_eq!(holes(r#""\{{x}""#), Some(3));
    }

    /// A hole holds a full expression, so everything an expression can contain
    /// has to be measured rather than scanned past (ADR-147 decision 1).
    #[test]
    fn a_hole_holds_a_full_expression() {
        for src in [
            r#""{a + b}""#,
            r#""{p.0}""#,
            r#""{xs.len()}""#,
            r#""{m["k"]}""#,
            r#""{if x { 1 } else { 2 }}""#,
            r#""{xs.map(|v| v * 2).sum()}""#,
        ] {
            assert!(closed(src), "{src}");
            assert!(holes(src).is_some(), "{src}");
        }
    }

    /// **The case a naive scanner gets wrong.** A `}` inside a nested string, a
    /// character literal, a template or a comment is not the end of the hole. A
    /// scanner that missed any of these would end the literal early and hand the
    /// lexer the rest of the line as source it was never written as.
    #[test]
    fn a_brace_inside_something_skipped_whole_is_not_the_end_of_a_hole() {
        for src in [
            r#""{m["}"]}""#,
            r#""{c == '}'}""#,
            r#""{parse(s, `{x:int}`)}""#,
            r#""{a /* } */ + b}""#,
            r#""{f("{y}")}""#,
        ] {
            assert!(closed(src), "{src}");
            assert_eq!(
                text_end(src, 0),
                TextEnd::Closed {
                    end: src.len(),
                    first_hole: Some(1)
                },
                "{src}"
            );
        }
    }

    /// A text literal ends at the line it opens on — the rule that predates
    /// interpolation. Every one of these is `T004` and one `TextLit`, which is
    /// byte for byte what it was before ADR-147 (decision 5).
    #[test]
    fn a_literal_that_does_not_close_on_its_line_is_unterminated() {
        // A hole that never closes.
        assert_eq!(
            text_end("\"a {b\ncd\"", 0),
            TextEnd::Unterminated { stopped: 5 }
        );
        // A quote that never closes, with no hole in it at all.
        assert_eq!(
            text_end("\"never closes\n", 0),
            TextEnd::Unterminated { stopped: 13 }
        );
        // A `//` in a hole eats the rest of the line.
        assert_eq!(
            text_end("\"{a // b}\"\n", 0),
            TextEnd::Unterminated { stopped: 4 }
        );
        // A block comment that spans a line takes the literal with it.
        assert_eq!(
            text_end("\"{a /* x\ny */}\"", 0),
            TextEnd::Unterminated { stopped: 8 }
        );
        // A trailing backslash is a dangling escape, not a continuation.
        assert_eq!(
            text_end("\"abc\\\ndef\"", 0),
            TextEnd::Unterminated { stopped: 5 }
        );
        // End of text with nothing after it.
        assert_eq!(text_end("\"{a}", 0), TextEnd::Unterminated { stopped: 4 });
    }

    /// Nesting is bounded, and past the bound the answer is an ordinary
    /// unterminated literal — never a literal measured under a second rule.
    #[test]
    fn nesting_is_bounded_and_the_bound_refuses_to_enter() {
        fn nested(n: usize) -> String {
            let mut s = String::new();
            for _ in 0..n {
                s.push_str("\"{");
            }
            s.push('x');
            for _ in 0..n {
                s.push_str("}\"");
            }
            s
        }
        let at_the_bound = nested(MAX_INTERPOLATION_NESTING);
        assert!(
            closed(&at_the_bound),
            "a literal nested exactly to the bound still closes"
        );
        let past = nested(MAX_INTERPOLATION_NESTING + 1);
        assert!(
            matches!(text_end(&past, 0), TextEnd::Unterminated { .. }),
            "one past the bound is an ordinary unterminated literal"
        );
        // And the pathological case terminates rather than recursing.
        let deep = "\"{".repeat(5_000);
        assert!(matches!(text_end(&deep, 0), TextEnd::Unterminated { .. }));
    }

    /// The resume path finds the same boundaries the pre-scan did, entered in
    /// the middle. This is the property that makes one function two readers.
    #[test]
    fn the_resume_path_walks_the_same_fragments() {
        let src = r#""a{x}b{y}c""#;
        //          0123456789
        assert_eq!(fragment_end(src, 0), Some(FragmentEnd::Hole(2)));
        assert_eq!(fragment_end(src, 4), Some(FragmentEnd::Hole(6)));
        assert_eq!(fragment_end(src, 8), Some(FragmentEnd::Close(src.len())));
    }

    #[test]
    fn adjacent_holes_leave_empty_fragments() {
        let src = r#""{a}{b}""#;
        assert_eq!(fragment_end(src, 0), Some(FragmentEnd::Hole(1)));
        assert_eq!(fragment_end(src, 3), Some(FragmentEnd::Hole(4)));
        assert_eq!(fragment_end(src, 6), Some(FragmentEnd::Close(src.len())));
    }

    #[test]
    fn a_multibyte_scalar_is_stepped_over_whole() {
        let src = "\"héllo {x} wörld\"";
        assert!(closed(src));
        let TextEnd::Closed { end, first_hole } = text_end(src, 0) else {
            panic!("expected closed");
        };
        assert!(src.is_char_boundary(end));
        assert!(src.is_char_boundary(first_hole.unwrap()));
        // An escape steps over a whole scalar, not a byte of one.
        assert!(closed("\"a\\λb\""));
    }
}
