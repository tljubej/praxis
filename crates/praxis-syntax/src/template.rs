//! Where a backtick template ends — **one** answer, for both scanners (D10).
//!
//! Two hand-written scanners have to agree about the extent of a
//! `` `…` `` run: [`praxis-parser`'s lexer], which turns it into one
//! `BacktickTemplate` token, and `praxis-input-parser`'s template scanner,
//! which re-reads that token's interior and has to find the same nested
//! templates and the same closing backtick inside it. When they were two
//! implementations of one rule they drifted immediately: the lexer counted
//! `{`/`}` everywhere and the scanner skipped string literals, so
//! `` `{c:one_of("{")}` `` — a legal §7.5 program, and one the scanner
//! accepted — left the lexer's brace counter above zero at the closing
//! backtick, which it then read as an *opener* and swallowed the rest of the
//! file into one token.
//!
//! `praxis-syntax` is the crate below both (it is where [`crate::ident`] and
//! [`crate::numeric`] already live for exactly this reason), so the rule lives
//! here and is called twice rather than written twice.
//!
//! # The rule
//!
//! Scanning starts just past the opening backtick and, until the run closes:
//!
//! - `\` hides the next scalar, so an escaped backtick cannot terminate a run.
//! - `{` opens a capture and `}` closes one — this is the only thing brace
//!   depth is for.
//! - `"` **inside a capture** opens a string literal, which is skipped whole:
//!   `one_of("{")`, `sep("}", int)` and `one_of("`")` all hold delimiters that
//!   are text, not structure. Outside a capture a quote is ordinary literal
//!   text (`` `He said "hi" {x:int}` ``), which is why the rule is conditioned
//!   on depth rather than applied everywhere.
//! - `` ` `` closes the run at capture depth 0. Inside a capture it *opens* a
//!   template of its own, because a capture body is a full parser expression
//!   (D10) and `` `{g:choice(A: `{x:int}`)}` `` is one template containing
//!   another.
//!
//! At most [`MAX_TEMPLATE_NESTING`] templates may nest; past that a backtick
//! simply closes, so adversarial input lands on the ordinary
//! unterminated/unexpected-token paths rather than on the stack. There is one
//! bound because there is one function — the two copies of this rule disagreed
//! about it by one level.
//!
//! # The quoted-run and scalar primitives
//!
//! A `"…"` inside a capture and a `'…'` inside an interpolation hole are the
//! same scan with a different closing byte, and both scanners have to step over
//! a multi-byte scalar the same way. [`quoted_run`] and [`skip_scalar`]
//! therefore live in this module — the lower of the two — and [`crate::interp`]
//! calls them instead of keeping a second copy, for the same reason this module
//! exists at all.
//!
//! [`praxis-parser`'s lexer]: https://docs.rs/praxis-parser

use crate::MAX_TEMPLATE_NESTING;

/// Where a `` `…` `` run ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TemplateEnd {
    /// The run closed. The index is **just past** the closing backtick, so
    /// `&src[open..end]` is the whole token, backticks included.
    Closed(usize),
    /// The line ended, or the text did, before the run closed.
    ///
    /// The index is where the run **stopped** — at the newline, or at the end
    /// of the text — so `&src[open..end]` is still a bounded token. That bound
    /// is the whole point of ADR-094: an unterminated template used to run to
    /// EOF, so `T002` covered the rest of the file and the `}` closing the
    /// enclosing block was swallowed inside the token, which produced a `P001`
    /// and a `Y001` after it. Three errors for one typo.
    Unterminated(usize),
}

/// Find the end of the backtick template whose opening backtick is at `open`.
///
/// `src[open]` must be `` ` ``.
///
/// # A template ends at the line it opens on (ADR-094)
///
/// A raw newline may not appear inside a template; `\n` is how §7.2 says a
/// template matches a line ending, and it is the only way. This is the rule a
/// `"…"` literal already follows — the backtick template was the one delimited
/// literal in the language that could silently span a line.
///
/// A raw newline never had a meaning here in any case. §7.2 lists literal text,
/// a space run, `\s*`, `\s+`, `\n`, `\t`, `\x20` and the ordinary escapes; a raw
/// newline is whitespace but is not a space, so it matched none of them and fell
/// through to *literal text*. Measured consequence: `` `{a:int}X⏎Y{b:int}` ``
/// matched LF input and **failed on CRLF**, while the `\n` escape matches both.
/// The multi-line template was a strictly weaker, silently CRLF-hostile shadow
/// of the construct §7.2 specifies.
#[must_use]
pub fn template_end(src: &str, open: usize) -> TemplateEnd {
    debug_assert_eq!(src.as_bytes().get(open), Some(&b'`'));
    match run(src.as_bytes(), open, 1) {
        Ok(end) => TemplateEnd::Closed(end),
        Err(stopped) => TemplateEnd::Unterminated(stopped),
    }
}

/// Find the end of the `"…"` literal whose opening quote is at `open`,
/// returning the index **just past** the closing quote, or `None` if the text
/// ends first. `\` hides the next scalar, so `"\""` is one literal.
#[must_use]
pub fn string_end(src: &str, open: usize) -> Option<usize> {
    debug_assert_eq!(src.as_bytes().get(open), Some(&b'"'));
    quoted_run(src.as_bytes(), open, b'"').ok()
}

/// One `` `…` `` run. `level` is 1 for the outermost template.
///
/// Byte-wise scanning is safe here because every delimiter is ASCII and no
/// UTF-8 continuation byte can be mistaken for one; the two places that step
/// over something unconditionally ([`skip_scalar`]) step over a whole scalar so
/// the returned index is always a character boundary.
/// `Ok(end)` is just past the closing backtick; `Err(stopped)` is where the run
/// gave up — at the newline that ended its line, or at the end of the text.
fn run(bytes: &[u8], open: usize, level: usize) -> Result<usize, usize> {
    let mut pos = open + 1; // past the opening backtick
    let mut braces = 0usize;
    while pos < bytes.len() {
        match bytes[pos] {
            // **A template ends at the line it opens on** (ADR-094). A `\r` is
            // taken with the `\n` it precedes so the token does not end mid-CRLF
            // and leave a stray `\r` for the next token to puzzle over.
            b'\n' => return Err(pos),
            b'\r' if bytes.get(pos + 1) == Some(&b'\n') => return Err(pos),
            // An escape hides the next scalar, but it cannot hide a line break:
            // `\` at the end of a line is a dangling escape, not a continuation,
            // and letting it swallow the newline would reintroduce exactly the
            // multi-line token this rule removes.
            b'\\' => {
                if matches!(bytes.get(pos + 1), Some(b'\n') | None)
                    || (bytes.get(pos + 1) == Some(&b'\r') && bytes.get(pos + 2) == Some(&b'\n'))
                {
                    return Err(pos + 1);
                }
                pos = skip_scalar(bytes, pos + 1);
            }
            // A quote is structure only inside a capture; in literal text it is
            // just a quote.
            b'"' if braces > 0 => pos = quoted_run(bytes, pos, b'"')?,
            b'{' => {
                braces += 1;
                pos += 1;
            }
            b'}' => {
                braces = braces.saturating_sub(1);
                pos += 1;
            }
            b'`' => {
                if braces == 0 || level >= MAX_TEMPLATE_NESTING {
                    return Ok(pos + 1);
                }
                // A nested run that hits the line end ends the outer run too,
                // and at the same place: one line, one token.
                pos = run(bytes, pos, level + 1)?;
            }
            _ => pos = skip_scalar(bytes, pos),
        }
    }
    Err(bytes.len())
}

/// One `'…'` or `"…"` run, honouring `\`. `bytes[open]` is the opening quote and
/// `terminator` is the byte that closes it; `Ok` is the index just past that
/// byte.
///
/// The two quotes are one rule with one byte different, so they are one
/// function: the `"…"` inside a template capture and the `'…'` inside an
/// interpolation hole (ADR-141) are measured by the same code.
///
/// Such a run is bounded by the same line rule as whatever holds it — the lexer
/// already refuses a raw newline inside a `"…"` literal, and there is no reason
/// for one nested in a capture or a hole to be different. `Err(stopped)`
/// therefore propagates straight out of [`run`], and out of `interp`'s `hole`.
///
/// A dangling `\` before a CRLF stops *at* the `\r`, exactly as [`run`] and
/// `interp`'s `fragment` do, so no token ever ends between a `\r` and the `\n`
/// it precedes. The two copies this replaced tested only for a bare `\n` there
/// and so stopped one byte later, mid-sequence — the single way they differed
/// from their own callers, and an oversight rather than a decision.
pub(crate) fn quoted_run(bytes: &[u8], open: usize, terminator: u8) -> Result<usize, usize> {
    let mut pos = open + 1;
    while pos < bytes.len() {
        match bytes[pos] {
            b'\n' => return Err(pos),
            b'\r' if bytes.get(pos + 1) == Some(&b'\n') => return Err(pos),
            b'\\' => {
                if matches!(bytes.get(pos + 1), Some(b'\n') | None)
                    || (bytes.get(pos + 1) == Some(&b'\r') && bytes.get(pos + 2) == Some(&b'\n'))
                {
                    return Err(pos + 1);
                }
                pos = skip_scalar(bytes, pos + 1);
            }
            b if b == terminator => return Ok(pos + 1),
            _ => pos = skip_scalar(bytes, pos),
        }
    }
    Err(bytes.len())
}

/// The index just past the whole UTF-8 scalar beginning at `pos`.
///
/// Every scanner in this crate that steps over a byte it does not care about
/// steps over a whole scalar instead, which is what keeps every index any of
/// them return a character boundary.
pub(crate) fn skip_scalar(bytes: &[u8], pos: usize) -> usize {
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

    /// `n` templates nested inside each other's captures, closed properly.
    fn nested(n: usize) -> String {
        let mut s = String::new();
        for _ in 0..n {
            s.push_str("`{a:");
        }
        s.push_str("int");
        for _ in 0..n {
            s.push_str("}`");
        }
        s
    }

    /// `n` nested templates whose innermost one holds a lone `"` as its literal
    /// text.
    ///
    /// That quote is *text* only if the innermost template is really entered as
    /// a template — at capture depth 0. If the bound stopped one level short,
    /// the same byte sits inside the parent's capture instead, where a quote
    /// opens a string literal that never closes. So this string closes at
    /// exactly `n <= MAX_TEMPLATE_NESTING` and at no larger `n`, which is what
    /// makes the bound observable.
    fn nested_quote(n: usize) -> String {
        let mut s = String::new();
        for _ in 0..n - 1 {
            s.push_str("`{a:");
        }
        s.push_str("`\"`");
        for _ in 0..n - 1 {
            s.push_str("}`");
        }
        s
    }

    fn closed(src: &str) -> bool {
        template_end(src, 0) == TemplateEnd::Closed(src.len())
    }

    /// **The blocker.** A `{`, `}` or backtick inside a string literal is text,
    /// not structure. The lexer's copy of this rule had no string arm, so
    /// `one_of("{")` left its brace counter above zero and the closing backtick
    /// read as an opener.
    #[test]
    fn a_delimiter_inside_a_string_is_text() {
        for src in [
            r#"`{c:one_of("{")}`"#,
            r#"`{c:one_of("}")}`"#,
            r#"`{s:sep("{", int)}`"#,
            r#"`{c:one_of("`")}`"#,
            r#"`{c:one_of("\"")}`"#,
            r#"`{c:one_of("{{{")}`"#,
        ] {
            assert!(closed(src), "{src}");
        }
    }

    /// Outside a capture a quote is ordinary literal text. Conditioning the
    /// string rule on depth is what keeps `` `He said "hi"` `` a template.
    #[test]
    fn a_quote_in_literal_text_is_not_a_string() {
        assert!(closed(r#"`He said "hi`"#));
        assert!(closed(r#"`" {x:int}`"#));
    }

    #[test]
    fn a_nested_template_is_part_of_the_run() {
        assert!(closed("`{g:choice(A: `{x:int}`, B: word)}`"));
        assert!(closed("`{a:choice(A: `{b:choice(C: `{c:int}`)}`)}`"));
        // An escaped backtick terminates nothing, at either depth.
        assert!(closed(r"`a\`b`"));
        assert!(closed(r"`{a:choice(A: `x\`y`)}`"));
    }

    #[test]
    fn a_run_that_never_closes_is_unterminated() {
        // The index is where the run stopped, which with no newline in the text
        // is its end — so `&src[open..end]` is still a bounded token.
        assert_eq!(
            template_end("`never closes", 0),
            TemplateEnd::Unterminated("`never closes".len())
        );
        assert_eq!(
            template_end("`{g:choice(A: `{x:int}`)}", 0),
            TemplateEnd::Unterminated("`{g:choice(A: `{x:int}`)}".len())
        );
        // An unterminated string swallows the rest, so the run cannot close.
        assert_eq!(
            template_end(r#"`{c:one_of("abc)}`"#, 0),
            TemplateEnd::Unterminated(r#"`{c:one_of("abc)}`"#.len())
        );
    }

    /// **ADR-094.** A template ends at the line it opens on, so an unterminated
    /// run stops at the newline instead of swallowing the rest of the file.
    ///
    /// That bound is the whole decision: `read \`{int\`` used to produce a
    /// `T002` covering everything after it, and because the `}` closing the
    /// enclosing block was inside the token, a `P001` and a `Y001` after that.
    ///
    /// Observed red without the `b'\n'` arm in `run`: every assertion here
    /// reports the length of the whole input instead of the first line's end.
    #[test]
    fn a_template_ends_at_the_line_it_opens_on() {
        // The run stops *at* the newline, so the token is `` `{int` `` and the
        // `}` on the next line is still the block's.
        assert_eq!(
            template_end("`{int\n}\n", 0),
            TemplateEnd::Unterminated(5),
            "the token is the first line's template, not the rest of the file"
        );
        // A closed template on one line is untouched.
        assert_eq!(template_end("`{a:int}`\nrest", 0), TemplateEnd::Closed(9));
        // CRLF: the run stops before the `\r`, so no token ends mid-sequence.
        assert_eq!(template_end("`{int\r\n}", 0), TemplateEnd::Unterminated(5));
        // A trailing backslash cannot swallow the line break — a dangling
        // escape is not a continuation, and treating it as one would reopen
        // exactly the multi-line token this rule removes.
        assert_eq!(
            template_end("`abc\\\ndef`", 0),
            TemplateEnd::Unterminated(5)
        );
        // A nested run that hits the line end ends the outer run too, at the
        // same place: one line, one token.
        assert_eq!(
            template_end("`{g:choice(A: `{x:int}\n)}`", 0),
            TemplateEnd::Unterminated(22)
        );
        // …and a string literal inside a capture is bounded by the same rule.
        assert_eq!(
            template_end("`{c:one_of(\"ab\n)}`", 0),
            TemplateEnd::Unterminated(14)
        );
        // A dangling `\` inside that string is not a continuation either, and it
        // stops at the line terminator's *first* byte — so a CRLF is not split.
        // That byte is the one place the separate string scanner disagreed with
        // `run` before the two became one `quoted_run`.
        assert_eq!(
            template_end("`{c:one_of(\"ab\\\ncd\")}`", 0),
            TemplateEnd::Unterminated(15)
        );
        assert_eq!(
            template_end("`{c:one_of(\"ab\\\r\ncd\")}`", 0),
            TemplateEnd::Unterminated(15)
        );
    }

    /// The bound is a bound, and it is exactly [`MAX_TEMPLATE_NESTING`]:
    /// `MAX_TEMPLATE_NESTING` nested templates are all entered, and the
    /// `MAX_TEMPLATE_NESTING + 1`-th is not.
    ///
    /// An *unbounded* implementation passes the first assertion and fails the
    /// second; a bound one level shorter or longer fails one of them. This is
    /// the assertion the predecessor test did not make: it fed the lexer 5,000
    /// unclosed openers and asserted only that *something* was reported, which
    /// a lexer with no nesting at all also does.
    #[test]
    fn nesting_is_bounded_at_max_template_nesting() {
        let at_the_bound = nested(MAX_TEMPLATE_NESTING);
        assert_eq!(
            template_end(&at_the_bound, 0),
            TemplateEnd::Closed(at_the_bound.len()),
            "a run nested exactly to the bound still closes at its own backtick"
        );

        let entered = nested_quote(MAX_TEMPLATE_NESTING);
        assert_eq!(
            template_end(&entered, 0),
            TemplateEnd::Closed(entered.len()),
            "the {MAX_TEMPLATE_NESTING}th template is entered, so its `\"` is literal text"
        );
        let not_entered = nested_quote(MAX_TEMPLATE_NESTING + 1);
        assert_eq!(
            template_end(&not_entered, 0),
            TemplateEnd::Unterminated(not_entered.len()),
            "one past the bound that template is not entered, so its `\"` is a string"
        );

        // And the pathological case terminates rather than recursing.
        let deep = "`{a:".repeat(5_000);
        assert_eq!(
            template_end(&deep, 0),
            TemplateEnd::Unterminated(deep.len())
        );
    }

    #[test]
    fn a_multibyte_scalar_after_a_backslash_is_stepped_over_whole() {
        // The escape skips `λ` entirely; the run still closes at its backtick,
        // and the returned index is a character boundary.
        let src = "`a\\λb`";
        assert_eq!(template_end(src, 0), TemplateEnd::Closed(src.len()));
        assert!(src.is_char_boundary(src.len()));
    }

    #[test]
    fn a_string_ends_at_its_own_unescaped_quote() {
        assert_eq!(string_end(r#""ab" rest"#, 0), Some(4));
        assert_eq!(string_end(r#""a\"b" rest"#, 0), Some(6));
        assert_eq!(string_end(r#""unterminated"#, 0), None);
    }
}
