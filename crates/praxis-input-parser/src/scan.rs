//! Interior re-scan of a backtick template (§7.2, §7.3).
//!
//! A `BacktickTemplate` token spans both backticks but its interior is opaque at
//! lex time. This module re-scans the interior into a sequence of
//! [`TemplatePart`]s: literal runs and `{...}` captures. Whitespace policy
//! escapes (`\s*`, `\s+`, `\n`, `\t`, `\x20`) are recognized here (§7.2).
//!
//! **A capture body is a full parser expression** (D10). `{items:csv(int)}` is
//! §7.7's own monkey example, so "atomics only" is not a smaller language — it
//! is a language that cannot run the design document's text. The body is parsed
//! *here*, by [`crate::body`], and not handed back to `praxis-parser`: ADR-023
//! fixes the dependency direction, and this crate must not depend on the
//! ordinary grammar.
//!
//! # The cursor (IP-01)
//!
//! The scanner used to walk `interior.as_bytes()` with a bare `usize` and turn
//! each ordinary byte into a `char` with `char::from(b)` — a Latin-1 decode, so
//! `λ=` came out as `Î»=`. Every position here is a **scalar** boundary with its
//! absolute byte offset in `interior`, and there is no `char::from(u8)` left to
//! split a multi-byte scalar.

use std::iter::Peekable;
use std::str::CharIndices;

use praxis_source::DiagCode;

use crate::ast::{TemplatePart, WsPolicy};
use crate::validate::ValidationError;

/// How deeply captures and nested templates may nest before the scanner
/// refuses (D10).
///
/// [`scan_template`] and [`crate::body::parse_capture_body`] are mutually
/// recursive once a capture body may hold a template of its own, so
/// `"{a:" + "{".repeat(100_000)` is adversarial input — and a *compiler* may
/// not answer adversarial input with a stack overflow. The bound is far above
/// anything a person writes.
pub use praxis_syntax::MAX_TEMPLATE_NESTING as MAX_NESTING;

/// An error encountered while scanning a template interior or a capture body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanError {
    /// An invalid escape sequence (e.g. `\q`). `seq` is **the text the source
    /// actually wrote**, sliced from it, not a re-`format!` of a guessed byte
    /// (IP-03).
    InvalidEscape { byte_offset: usize, seq: String },
    /// An unterminated capture `{...` (no closing `}`).
    UnterminatedCapture { byte_offset: usize },
    /// An empty capture `{}`.
    EmptyCapture { byte_offset: usize },
    /// A capture whose name is not an identifier (§4.1, IP-04).
    InvalidCaptureName { byte_offset: usize, name: String },
    /// A capture body naming a parser that does not exist (IP-06).
    UnknownCaptureKind { byte_offset: usize, name: String },
    /// A capture body calling a constructor that does not exist (§7.5).
    UnknownConstructor { byte_offset: usize, name: String },
    /// A capture body that is not a parser expression at all.
    MalformedCaptureBody { byte_offset: usize, message: String },
    /// A constructor call in a capture body whose arguments do not have §7.5's
    /// shape. Carries the [`ValidationError`] `check_call` produced, so the
    /// code and the message are the same ones a top-level call would report.
    CallShape(ValidationError),
    /// Nesting past [`MAX_NESTING`].
    ///
    /// `what` is **which** nesting hit the bound — templates, `{`, or `(`. The
    /// message named templates for all three, so `csv(` thirty-three deep was
    /// reported as "template nesting is deeper than 32" for text with one
    /// template in it. A diagnostic that states the wrong thing is worse than a
    /// vague one.
    NestingTooDeep {
        byte_offset: usize,
        what: &'static str,
    },
}

impl ScanError {
    /// The byte offset in the scanned text this error is anchored at.
    #[must_use]
    pub fn byte_offset(&self) -> usize {
        match self {
            ScanError::InvalidEscape { byte_offset, .. }
            | ScanError::UnterminatedCapture { byte_offset }
            | ScanError::EmptyCapture { byte_offset }
            | ScanError::InvalidCaptureName { byte_offset, .. }
            | ScanError::UnknownCaptureKind { byte_offset, .. }
            | ScanError::UnknownConstructor { byte_offset, .. }
            | ScanError::MalformedCaptureBody { byte_offset, .. }
            | ScanError::NestingTooDeep { byte_offset, .. } => *byte_offset,
            ScanError::CallShape(err) => err.span.start().to_u32() as usize,
        }
    }

    /// The same error, anchored `delta` bytes later.
    ///
    /// A nested template's interior is scanned in **its own** offsets: the
    /// scanner is handed the text between the backticks and knows nothing about
    /// where that text sits in the template that contains it. The caller does
    /// know, and this is how the two are joined. Without it every caret under a
    /// nested template lands short by the nested interior's own offset — the
    /// error for `` `{a:sections(x: `{y:csv(int, int)}`)}` `` pointed at
    /// `sections`.
    #[must_use]
    pub fn shifted(self, delta: usize) -> ScanError {
        let bump = |at: usize| at + delta;
        match self {
            ScanError::InvalidEscape { byte_offset, seq } => ScanError::InvalidEscape {
                byte_offset: bump(byte_offset),
                seq,
            },
            ScanError::UnterminatedCapture { byte_offset } => ScanError::UnterminatedCapture {
                byte_offset: bump(byte_offset),
            },
            ScanError::EmptyCapture { byte_offset } => ScanError::EmptyCapture {
                byte_offset: bump(byte_offset),
            },
            ScanError::InvalidCaptureName { byte_offset, name } => ScanError::InvalidCaptureName {
                byte_offset: bump(byte_offset),
                name,
            },
            ScanError::UnknownCaptureKind { byte_offset, name } => ScanError::UnknownCaptureKind {
                byte_offset: bump(byte_offset),
                name,
            },
            ScanError::UnknownConstructor { byte_offset, name } => ScanError::UnknownConstructor {
                byte_offset: bump(byte_offset),
                name,
            },
            ScanError::MalformedCaptureBody {
                byte_offset,
                message,
            } => ScanError::MalformedCaptureBody {
                byte_offset: bump(byte_offset),
                message,
            },
            ScanError::NestingTooDeep { byte_offset, what } => ScanError::NestingTooDeep {
                byte_offset: bump(byte_offset),
                what,
            },
            ScanError::CallShape(mut err) => {
                err.span = err.span.shifted(delta as u32);
                ScanError::CallShape(err)
            }
        }
    }

    /// The diagnostic this error is reported under.
    ///
    /// **Exhaustive on purpose** (IP-06). Every `ScanError` used to be
    /// flattened into `DiagCode::TemplateScan` (I030) by one `err_diag` call in
    /// `praxis-hir`, so the codes ADR-051 allocated for these cases —
    /// `InvalidCaptureName` I011, `UnknownCaptureKind` I012,
    /// `UnknownConstructor` I013 — were constructed nowhere in the tree. A
    /// `match` with no wildcard is what stops the next variant from silently
    /// inheriting I030.
    #[must_use]
    pub fn code(&self) -> DiagCode {
        match self {
            ScanError::InvalidCaptureName { .. } => DiagCode::InvalidCaptureName,
            ScanError::UnknownCaptureKind { .. } => DiagCode::UnknownCaptureKind,
            ScanError::UnknownConstructor { .. } => DiagCode::UnknownConstructor,
            ScanError::CallShape(err) => err.code,
            ScanError::InvalidEscape { .. }
            | ScanError::UnterminatedCapture { .. }
            | ScanError::EmptyCapture { .. }
            | ScanError::MalformedCaptureBody { .. }
            | ScanError::NestingTooDeep { .. } => DiagCode::TemplateScan,
        }
    }
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScanError::InvalidEscape { byte_offset, seq } => {
                write!(f, "invalid escape `{seq}` at byte {byte_offset}")
            }
            ScanError::UnterminatedCapture { byte_offset } => {
                write!(f, "unterminated capture starting at byte {byte_offset}")
            }
            ScanError::EmptyCapture { byte_offset } => {
                write!(f, "empty capture `{{}}` at byte {byte_offset}")
            }
            ScanError::InvalidCaptureName { byte_offset, name } => write!(
                f,
                "`{name}` at byte {byte_offset} is not a capture name: a capture name is an \
                 identifier (§4.1)"
            ),
            ScanError::UnknownCaptureKind { byte_offset, name } => write!(
                f,
                "unknown parser `{name}` at byte {byte_offset}: no atomic or constructor is \
                 spelled that way (§7.4, §7.5)"
            ),
            ScanError::UnknownConstructor { byte_offset, name } => {
                write!(
                    f,
                    "unknown parser constructor `{name}` at byte {byte_offset} (§7.5)"
                )
            }
            ScanError::MalformedCaptureBody {
                byte_offset,
                message,
            } => write!(f, "malformed capture body at byte {byte_offset}: {message}"),
            ScanError::CallShape(err) => f.write_str(&err.message),
            // **The number is the number that is enforced**, and `what` is
            // what was counted. The message named templates and `MAX_NESTING`
            // whatever tripped — including a parenthesis bound, and including a
            // template bound that fired at half that depth because the two
            // mutually recursive halves each added one per level.
            ScanError::NestingTooDeep { byte_offset, what } => write!(
                f,
                "{what} nesting is deeper than {MAX_NESTING} at byte {byte_offset}"
            ),
        }
    }
}

impl std::error::Error for ScanError {}

// ===========================================================================
// The scalar cursor (IP-01).
// ===========================================================================

/// A cursor over the scalars of a `&str`, each with its absolute byte offset.
///
/// The invariant: [`Scan::pos`] is always a UTF-8 boundary in [`Scan::src`], so
/// `&src[a..cur.pos()]` is always a valid slice and a multi-byte scalar can
/// never be split.
pub(crate) struct Scan<'a> {
    src: &'a str,
    iter: Peekable<CharIndices<'a>>,
    pos: usize,
}

impl<'a> Scan<'a> {
    pub(crate) fn new(src: &'a str) -> Self {
        Scan {
            src,
            iter: src.char_indices().peekable(),
            pos: 0,
        }
    }

    /// The next scalar and its offset, without consuming it.
    pub(crate) fn peek(&mut self) -> Option<(usize, char)> {
        self.iter.peek().copied()
    }

    /// The next scalar's value, without consuming it.
    pub(crate) fn peek_char(&mut self) -> Option<char> {
        self.peek().map(|(_, c)| c)
    }

    /// Consume and return the next scalar and its offset.
    pub(crate) fn bump(&mut self) -> Option<(usize, char)> {
        let next = self.iter.next();
        self.pos = match next {
            Some((at, c)) => at + c.len_utf8(),
            None => self.src.len(),
        };
        next
    }

    /// Consume the next scalar iff it is `c`.
    pub(crate) fn eat(&mut self, c: char) -> bool {
        if self.peek_char() == Some(c) {
            self.bump();
            true
        } else {
            false
        }
    }

    /// The byte offset just past the last consumed scalar.
    pub(crate) fn pos(&self) -> usize {
        self.pos
    }

    /// The source this cursor walks.
    pub(crate) fn src(&self) -> &'a str {
        self.src
    }

    /// Advance to `byte`, which must be at or after the current position and on
    /// a scalar boundary. This is how a run whose extent was decided by
    /// [`praxis_syntax::template`] — one byte index — is handed back to a
    /// cursor that walks scalars.
    pub(crate) fn advance_to(&mut self, byte: usize) {
        while self.pos < byte && self.bump().is_some() {}
    }
}

// ===========================================================================
// scan_template
// ===========================================================================

/// Re-scan the interior of a backtick template into template parts.
///
/// `interior` is the text *between* the backticks (the caller strips them). The
/// returned parts alternate between literal runs and captures; consecutive
/// literal characters are coalesced into one `Literal` part with the whitespace
/// policy of the run. Spans are **relative to `interior`**; the HIR bridge
/// rebases them onto the file by the template token's start + 1.
///
/// # Errors
/// Returns [`ScanError`] on a malformed template: a bad escape, an unterminated
/// or empty capture, a capture name that is not an identifier, or a capture
/// body that is not a parser expression.
pub fn scan_template(interior: &str) -> Result<Vec<TemplatePart>, ScanError> {
    scan_template_at(interior, 0)
}

/// [`scan_template`] with the current nesting depth (D10).
///
/// `depth` is **how many templates are already open**, so the outermost is
/// scanned at `0` and the bound is `MAX_NESTING` levels — the same count, and
/// the same limit, as `praxis_syntax::template::template_end`, which is what
/// decides how much text the lexer hands over in the first place.
///
/// It used to be incremented at *both* hops of the mutual recursion — here for
/// the capture body, and again in `body::parse_expr` for the nested template —
/// so one template level cost two, the scanner refused at seventeen levels, and
/// the message still named thirty-two. A stricter inner bound would have been
/// defensible; a diagnostic naming a limit nothing enforces is not.
pub(crate) fn scan_template_at(
    interior: &str,
    depth: usize,
) -> Result<Vec<TemplatePart>, ScanError> {
    if depth >= MAX_NESTING {
        return Err(ScanError::NestingTooDeep {
            byte_offset: 0,
            what: "template",
        });
    }
    let mut parts = Vec::new();
    let mut lit = String::new();
    let mut cur = Scan::new(interior);

    while let Some((at, c)) = cur.peek() {
        match c {
            '{' => {
                flush(&mut lit, &mut parts);
                let (name, body_text, body_at) = capture_extent(&mut cur, at)?;
                let name = match name {
                    Some(raw) => Some(capture_name(raw, at)?),
                    None => None,
                };
                // **Not `depth + 1`.** A capture body is inside *this*
                // template, not inside a further one; the level is added where
                // a level is entered, which is `body::parse_expr`'s backtick
                // arm calling back into here.
                let parser = crate::body::parse_capture_body(body_text, body_at, depth)?;
                parts.push(TemplatePart::Capture {
                    name,
                    parser: Box::new(parser),
                });
            }
            '\\' => {
                // **No guard** (IP-02). A backslash at the very end used to
                // fail the `i + 1 < bytes.len()` test on the arm itself and
                // fall through to the ordinary-character arm, so `"prefix\"`
                // ended up as the literal text `prefix\`. End-of-input is a
                // case *inside* the escape handler now, which is what makes the
                // handler total.
                cur.bump();
                match escape(&mut cur, at)? {
                    Escape::Policy(ws) => {
                        flush(&mut lit, &mut parts);
                        parts.push(TemplatePart::Literal {
                            text: String::new(),
                            ws,
                        });
                    }
                    Escape::Char(ch) => lit.push(ch),
                }
            }
            _ => {
                cur.bump();
                lit.push(c);
            }
        }
    }
    flush(&mut lit, &mut parts);
    Ok(parts)
}

/// Flush the accumulated literal run, turning a space/tab run at **either** end
/// of it into a whitespace policy part.
///
/// **A space/tab run at either end of a literal is flexible whitespace, not
/// text** (REP-20). §7.2's rule is that an ordinary run of spaces or tabs is
/// flexible, and a literal's own policy is applied before its bytes are
/// matched. Leaving a run in the text made it *also* exact, so it had to be
/// matched twice:
///
///   - The leading run could never match at all. §3.3's own
///     `` `{x1:int},{y1:int} -> {x2:int},{y2:int}` `` failed at the `-` of `->`
///     on every input, because the policy consumed the space and then the
///     literal " -> " was compared against "-> ".
///   - The trailing run made the spacing rigid on one side only: `1 -> 2`
///     matched and `1->2` did not.
///
/// # Each end is its own part (REP-20's trailing half)
///
/// A literal has one policy slot and it sits in front of the text, so one
/// `Literal` part cannot carry a run on both sides. The predecessor stripped
/// both ends with a single `trim_matches` and derived a policy from the leading
/// end only — so the **trailing** run was scanned away and represented nowhere.
/// It was neither required nor consumed: `` `x: {a:rest}` `` matched `x:hello`,
/// and over `x: hello` it handed `rest` the space the template had written.
/// A silent wrong answer, and REP-20's row claimed it fixed.
///
/// The trailing run therefore becomes **its own part** — an empty literal
/// carrying `SpaceRun`, which is the representation `` `{a:int} {b:int}` ``
/// already lowers to and every consumer already handles. `x: ` is
/// `Literal{"x:", None}` then `Literal{"", SpaceRun}`.
///
/// The strip's old justification — "every capture consumes leading whitespace
/// before it walks" — is a premise S20 deleted: a capture is offered the bytes
/// at the cursor, whitespace and all, and `walk_atomic` decides. `int` and
/// `word` still self-trim, which is why the defect stayed invisible; `char`,
/// `text` and `rest` do not, and they are what it showed up in.
///
/// A literal that strips to **nothing** is one run, not two, and emits one
/// part: counting it at both ends would make `` `{a:int} {b:int}` `` demand two
/// separate whitespace runs between the captures.
///
/// A run *inside* a literal is untouched — nothing else consumes it, so it stays
/// exact — and `\x20` is the escape for a space that must be matched literally.
/// (`\x20` never reaches here as text: [`escape`] returns it as a policy part of
/// its own, so an exact space cannot be mistaken for a run.)
///
/// **The policy is derived, not assumed** (IPR-12). Every literal used to be
/// tagged `SpaceRun` unconditionally, including literals with no whitespace run
/// anywhere near them — so the runtime could not distinguish "the template
/// wrote a space here" from "it did not", and the only way to keep
/// `{a:int},{b:int}` matching was to implement `SpaceRun` as zero-or-more,
/// which is not what `SpaceRun` means. A literal that had a run stripped from
/// its **front** carries `SpaceRun`; one that did not carries
/// [`WsPolicy::None`], and then `SpaceRun` can require the one-or-more its own
/// definition promises — at both ends.
fn flush(lit: &mut String, parts: &mut Vec<TemplatePart>) {
    if lit.is_empty() {
        return;
    }
    let text = std::mem::take(lit);
    let had_leading_run = text.starts_with([' ', '\t']);
    let stripped = text.trim_matches([' ', '\t']);
    // A literal whose runs strip it to nothing is pure whitespace: one run, one
    // part, and the policy *is* the part. `had_leading_run` is true here by
    // construction (a non-empty all-whitespace text starts with whitespace), so
    // the single part carries the policy.
    let had_trailing_run = !stripped.is_empty() && text.ends_with([' ', '\t']);
    parts.push(TemplatePart::Literal {
        text: stripped.to_string(),
        ws: if had_leading_run {
            WsPolicy::SpaceRun
        } else {
            WsPolicy::None
        },
    });
    if had_trailing_run {
        // The trailing run has no slot on the literal it followed — the slot is
        // in front of the text — so it gets a part of its own.
        parts.push(TemplatePart::Literal {
            text: String::new(),
            ws: WsPolicy::SpaceRun,
        });
    }
}

/// What one escape sequence produced.
enum Escape {
    /// A whitespace policy part (`\s*`, `\s+`, `\n`, `\t`, `\x20`).
    Policy(WsPolicy),
    /// An escaped literal character (`` \` ``, `\\`).
    Char(char),
}

/// Decode the escape whose backslash was at `at` and has already been consumed.
///
/// **Total on end of input** (IP-02) and **honest about what it read** (IP-03):
/// the reported `seq` is `&interior[at..cur.pos()]` — the text the source
/// actually wrote. The predecessor built the message with
/// `format!("\\s{}", char::from(next))` where `next` was the byte that *chose*
/// the arm, so every bad `\s?` reported `\ss`, and every bad `\xNN` reported
/// just `\x`.
fn escape(cur: &mut Scan<'_>, at: usize) -> Result<Escape, ScanError> {
    let invalid = |cur: &Scan<'_>| ScanError::InvalidEscape {
        byte_offset: at,
        seq: cur.src()[at..cur.pos()].to_string(),
    };

    let Some((_, c)) = cur.bump() else {
        // A backslash at the very end of the interior. `seq` is `\`.
        return Err(invalid(cur));
    };
    match c {
        's' => {
            if cur.eat('*') {
                Ok(Escape::Policy(WsPolicy::ZeroOrMore))
            } else if cur.eat('+') {
                Ok(Escape::Policy(WsPolicy::OneOrMore))
            } else {
                // Consume whatever did follow, so the message shows it.
                cur.bump();
                Err(invalid(cur))
            }
        }
        'n' => Ok(Escape::Policy(WsPolicy::Newline)),
        't' => Ok(Escape::Policy(WsPolicy::Tab)),
        'x' => {
            // `\x20` is §7.2's exact-space escape. Consume up to two more
            // scalars either way, so `\x2` at the end reports `\x2` and `\x21`
            // reports `\x21` rather than both reporting `\x`.
            for _ in 0..2 {
                if cur.peek().is_some() {
                    cur.bump();
                }
            }
            if &cur.src()[at..cur.pos()] == "\\x20" {
                Ok(Escape::Policy(WsPolicy::ExactSpace))
            } else {
                Err(invalid(cur))
            }
        }
        '`' | '\\' => Ok(Escape::Char(c)),
        _ => Err(invalid(cur)),
    }
}

/// Consume a capture starting at the `{` the cursor is sitting on, returning
/// `(optional name text, body text, body's byte offset)`.
///
/// **The `}`-scan is depth-aware** (D10). A capture body may hold `}` inside a
/// string (`{c:one_of("}")}`), `)` and `,` inside a call (`{xs:sep(",", int)}`),
/// and a template of its own. The predecessor scanned to the *first* `}`, which
/// is why the grammar had to be "atomics only" — and §7.7's own monkey example
/// writes `{items:csv(int)}`.
fn capture_extent<'a>(
    cur: &mut Scan<'a>,
    open: usize,
) -> Result<(Option<&'a str>, &'a str, usize), ScanError> {
    cur.bump(); // the `{`
    let body_at = cur.pos();
    let mut braces = 1usize;
    let mut parens = 0usize;
    // The offset of the `:` that separates the name from the parser, if one
    // occurs at nesting depth zero. `{g:choice(A: word)}`'s inner colon is not
    // one, and neither is the colon in `{c:one_of(":")}`.
    let mut name_colon = None;

    let close = loop {
        let Some((at, c)) = cur.peek() else {
            return Err(ScanError::UnterminatedCapture { byte_offset: open });
        };
        match c {
            '"' => skip_string(cur)?,
            '`' => {
                take_template(cur)?;
            }
            '\\' => {
                cur.bump();
                cur.bump();
            }
            '{' => {
                braces += 1;
                if braces > MAX_NESTING {
                    return Err(ScanError::NestingTooDeep {
                        byte_offset: at,
                        what: "`{`",
                    });
                }
                cur.bump();
            }
            '}' => {
                braces -= 1;
                cur.bump();
                if braces == 0 {
                    break at;
                }
            }
            '(' => {
                parens += 1;
                if parens > MAX_NESTING {
                    return Err(ScanError::NestingTooDeep {
                        byte_offset: at,
                        what: "`(`",
                    });
                }
                cur.bump();
            }
            ')' => {
                if parens == 0 {
                    return Err(ScanError::MalformedCaptureBody {
                        byte_offset: at,
                        message: "unbalanced `)`".to_string(),
                    });
                }
                parens -= 1;
                cur.bump();
            }
            ':' => {
                if braces == 1 && parens == 0 && name_colon.is_none() {
                    name_colon = Some(at);
                }
                cur.bump();
            }
            _ => {
                cur.bump();
            }
        }
    };

    if parens != 0 {
        return Err(ScanError::MalformedCaptureBody {
            byte_offset: open,
            message: "unbalanced `(`".to_string(),
        });
    }

    let src = cur.src();
    let body = &src[body_at..close];
    if body.trim().is_empty() {
        return Err(ScanError::EmptyCapture { byte_offset: open });
    }
    match name_colon {
        Some(colon) => Ok((
            Some(&src[body_at..colon]),
            &src[colon + 1..close],
            colon + 1,
        )),
        None => Ok((None, body, body_at)),
    }
}

/// Consume a `"…"` run, honouring `\\`. The cursor is on the opening quote.
///
/// The rule is [`praxis_syntax::template::string_end`]'s, not a second copy of
/// it: the lexer has to skip the same literals when it decides where this
/// template's token ends.
fn skip_string(cur: &mut Scan<'_>) -> Result<(), ScanError> {
    let open = cur.pos();
    match praxis_syntax::template::string_end(cur.src(), open) {
        Some(end) => {
            cur.advance_to(end);
            Ok(())
        }
        None => Err(ScanError::MalformedCaptureBody {
            byte_offset: open,
            message: "unterminated string literal".to_string(),
        }),
    }
}

/// Consume a nested `` `…` `` template run and return its **interior** (the
/// text between the backticks). The cursor is on the opening backtick.
///
/// **The extent rule is [`praxis_syntax::template::template_end`]'s**, the same
/// one the lexer applies when it decides where the enclosing token ends. This
/// function used to be a second implementation of it — one that counted `{`/`}`
/// but knew about strings, against a lexer that counted `{`/`}` and did not —
/// and the two also bounded nesting at depths that differed by one. There is
/// one bound because there is one function.
pub(crate) fn take_template<'a>(cur: &mut Scan<'a>) -> Result<&'a str, ScanError> {
    let open = cur.pos();
    match praxis_syntax::template::template_end(cur.src(), open) {
        praxis_syntax::template::TemplateEnd::Closed(end) => {
            cur.advance_to(end);
            Ok(&cur.src()[open + 1..end - 1])
        }
        praxis_syntax::template::TemplateEnd::Unterminated(_) => {
            Err(ScanError::MalformedCaptureBody {
                byte_offset: open,
                message: "unterminated nested template".to_string(),
            })
        }
    }
}

/// Validate a capture's name against the language's **one** identifier class
/// (§4.1, IP-04).
///
/// The predecessor used a local ASCII rule (`is_ascii_alphabetic`), so `{λ:int}`
/// was not recognized as a named capture at all — the whole body `λ:int` was
/// reinterpreted as the parser expression, which then failed for an unrelated
/// reason. A name the lexer would not have produced is now *reported*, never
/// silently re-read as something else.
fn capture_name(raw: &str, at: usize) -> Result<crate::ast::CaptureName, ScanError> {
    crate::ast::CaptureName::parse(raw.trim()).map_err(|_| ScanError::InvalidCaptureName {
        byte_offset: at,
        name: raw.trim().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{AtomicKind, ParserAst};

    fn literals(parts: &[TemplatePart]) -> Vec<&str> {
        parts
            .iter()
            .filter_map(|p| match p {
                TemplatePart::Literal { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn policies(parts: &[TemplatePart]) -> Vec<WsPolicy> {
        parts
            .iter()
            .filter_map(|p| match p {
                TemplatePart::Literal { ws, .. } => Some(*ws),
                _ => None,
            })
            .collect()
    }

    fn capture_kind(part: &TemplatePart) -> AtomicKind {
        match part {
            TemplatePart::Capture { parser, .. } => match parser.as_ref() {
                ParserAst::Atomic { kind, .. } => *kind,
                other => panic!("expected an atomic capture, got {other:?}"),
            },
            other => panic!("expected a capture, got {other:?}"),
        }
    }

    #[test]
    fn plain_literal_template() {
        let parts = scan_template("hello").unwrap();
        assert_eq!(parts.len(), 1);
        match &parts[0] {
            TemplatePart::Literal { text, .. } => assert_eq!(text, "hello"),
            _ => panic!("expected literal"),
        }
    }

    #[test]
    fn single_anonymous_capture() {
        let parts = scan_template("{int}").unwrap();
        assert_eq!(parts.len(), 1);
        match &parts[0] {
            TemplatePart::Capture { name, .. } => assert!(name.is_none()),
            _ => panic!("expected capture"),
        }
    }

    #[test]
    fn named_capture_with_literal() {
        let parts = scan_template("{x:int},{y:int}").unwrap();
        assert_eq!(parts.len(), 3);
        match &parts[0] {
            TemplatePart::Capture { name, .. } => {
                assert_eq!(name.as_ref().map(|n| n.as_str()), Some("x"));
            }
            _ => panic!("expected capture"),
        }
        match &parts[1] {
            TemplatePart::Literal { text, .. } => assert_eq!(text, ","),
            _ => panic!("expected literal"),
        }
        match &parts[2] {
            TemplatePart::Capture { name, .. } => {
                assert_eq!(name.as_ref().map(|n| n.as_str()), Some("y"));
            }
            _ => panic!("expected capture"),
        }
    }

    /// **REP-20 at the unit level.** A space/tab run at either end of a literal
    /// becomes the whitespace policy and leaves the text — the leading run as
    /// the literal's own `ws`, the trailing run as an empty literal of its own,
    /// because a literal has one policy slot and it sits in front of the text.
    ///
    /// Here as well as in the JIT tests because this is the *scanner's* rule: the
    /// interpreter honours a policy it is given, and the defect was that the same
    /// spaces were also in the bytes it had to match.
    #[test]
    fn a_literals_edge_whitespace_is_its_policy_and_not_its_text() {
        // §3.3's own template. The middle literal is `->`, not ` -> ` and not
        // `-> `: the run on each side is a policy, and the trailing one is a
        // part of its own.
        let parts = scan_template("{x1:int},{y1:int} -> {x2:int},{y2:int}").unwrap();
        assert_eq!(literals(&parts), vec![",", "->", "", ","]);
        // …and the policy says which of them had a run in front of it (IPR-12).
        // A comma the template wrote with nothing before it must not match an
        // input that has a space there.
        assert_eq!(
            policies(&parts),
            vec![
                WsPolicy::None,
                WsPolicy::SpaceRun,
                WsPolicy::SpaceRun,
                WsPolicy::None
            ],
            "only the literals a run was written against carry SpaceRun"
        );

        // Both ends, and a run *inside* a literal, which stays exact — nothing else
        // consumes it, and `\\x20` is the escape for a space that must match.
        let parts = scan_template("{a:int} a b {b:int}").unwrap();
        match &parts[1] {
            TemplatePart::Literal { text, ws } => {
                assert_eq!(text, "a b");
                assert_eq!(*ws, WsPolicy::SpaceRun);
            }
            _ => panic!("expected literal"),
        }
        // The trailing run of that same literal, as its own part.
        match &parts[2] {
            TemplatePart::Literal { text, ws } => {
                assert!(text.is_empty());
                assert_eq!(*ws, WsPolicy::SpaceRun);
            }
            _ => panic!("expected the trailing run's part"),
        }

        // **A trailing run with no leading one is still a policy** — this is the
        // half REP-20's first round dropped. `x: ` is `"x:"` with no policy,
        // then the run.
        let parts = scan_template("x: {a:rest}").unwrap();
        assert_eq!(literals(&parts), vec!["x:", ""]);
        assert_eq!(
            policies(&parts),
            vec![WsPolicy::None, WsPolicy::SpaceRun],
            "the run after `x:` is the policy, not text, and not nothing"
        );

        // A literal that is only whitespace strips to nothing and is still emitted:
        // the policy is the part — **one** part. Counting it as a leading run and
        // a trailing run would make this template demand two separate runs.
        let parts = scan_template("{a:int} {b:int}").unwrap();
        assert_eq!(parts.len(), 3);
        match &parts[1] {
            TemplatePart::Literal { text, ws } => {
                assert!(text.is_empty());
                assert_eq!(*ws, WsPolicy::SpaceRun);
            }
            _ => panic!("expected literal"),
        }

        // An escaped policy is untouched: it carries no text to strip.
        let parts = scan_template(r"{a:int}\s+{b:int}").unwrap();
        match &parts[1] {
            TemplatePart::Literal { text, ws } => {
                assert!(text.is_empty());
                assert_eq!(*ws, WsPolicy::OneOrMore);
            }
            _ => panic!("expected ws literal"),
        }
    }

    #[test]
    fn whitespace_escape_policies() {
        let parts = scan_template("a\\s*b").unwrap();
        assert_eq!(parts.len(), 3);
        match &parts[1] {
            TemplatePart::Literal { ws, .. } => assert_eq!(*ws, WsPolicy::ZeroOrMore),
            _ => panic!("expected ws literal"),
        }
    }

    #[test]
    fn unterminated_capture_errors() {
        assert!(matches!(
            scan_template("{int"),
            Err(ScanError::UnterminatedCapture { .. })
        ));
    }

    #[test]
    fn empty_capture_errors() {
        assert!(matches!(
            scan_template("{}"),
            Err(ScanError::EmptyCapture { .. })
        ));
    }

    #[test]
    fn escaped_backtick_is_literal() {
        let parts = scan_template("a\\`b").unwrap();
        assert_eq!(parts.len(), 1);
        match &parts[0] {
            TemplatePart::Literal { text, .. } => assert_eq!(text, "a`b"),
            _ => panic!("expected literal"),
        }
    }

    /// **IP-01.** Ordinary template text used to be copied byte by byte through
    /// `char::from(b)`, which is a Latin-1 decode: `λ=` became `Î»=`.
    #[test]
    fn regression_unicode_literal_text_is_preserved() {
        let parts = scan_template("λ={int}").unwrap();
        match &parts[0] {
            TemplatePart::Literal { text, .. } => assert_eq!(text, "λ="),
            _ => panic!("expected literal"),
        }
    }

    /// **IP-02.** The escape arm was guarded on there being a next byte, so a
    /// terminal backslash fell through to the ordinary-character arm and became
    /// literal text. Note the offset: the error is anchored at the backslash.
    #[test]
    fn regression_trailing_backslash_is_an_invalid_escape() {
        assert!(matches!(
            scan_template("prefix\\"),
            Err(ScanError::InvalidEscape { byte_offset: 6, .. })
        ));
    }

    /// **IP-03.** The message used to be rebuilt from the byte that *selected*
    /// the arm rather than sliced from the source, so every bad `\s?` reported
    /// `\ss` and every bad `\xNN` reported `\x`. `seq` is now the exact source
    /// substring, which is a property the assertions below can compare against
    /// the input directly.
    #[test]
    fn an_invalid_escape_reports_the_sequence_the_source_actually_wrote() {
        for (src, expected) in [
            (r"a\sq", r"\sq"),
            (r"a\s", r"\s"),
            (r"a\x2", r"\x2"),
            (r"a\x21", r"\x21"),
            (r"a\q", r"\q"),
            ("a\\", "\\"),
            // A non-ASCII scalar after the backslash is one scalar, not two
            // bytes — the byte walk would have sliced it in half.
            (r"a\λ", r"\λ"),
        ] {
            match scan_template(src) {
                Err(ScanError::InvalidEscape { seq, byte_offset }) => {
                    assert_eq!(seq, expected, "for {src:?}");
                    assert_eq!(
                        &src[byte_offset..byte_offset + seq.len()],
                        seq,
                        "`seq` must be the source's own text at `byte_offset`, for {src:?}"
                    );
                }
                other => panic!("{src:?} must be an invalid escape, got {other:?}"),
            }
        }
        // And the valid ones are still valid.
        assert!(scan_template(r"a\x20b").is_ok());
        assert!(scan_template(r"a\s*b").is_ok());
        assert!(scan_template(r"a\s+b").is_ok());
    }

    /// **IP-04.** A capture name is the language's own identifier (§4.1), not a
    /// local ASCII rule. `{λ:int}` used to be read as an *anonymous* capture
    /// whose parser expression was the whole text `λ:int`.
    #[test]
    fn a_capture_name_is_the_languages_own_identifier() {
        for (src, name) in [
            ("{λ:int}", "λ"),
            ("{日本語:int}", "日本語"),
            ("{_x9:int}", "_x9"),
        ] {
            let parts = scan_template(src).unwrap();
            match &parts[0] {
                TemplatePart::Capture { name: got, .. } => {
                    assert_eq!(got.as_ref().map(|n| n.as_str()), Some(name), "for {src}");
                }
                other => panic!("{src} must be a named capture, got {other:?}"),
            }
        }

        // A name that is not an identifier is reported, not silently reread as
        // an anonymous capture over the whole body.
        for src in ["{9x:int}", "{a b:int}", "{:int}", "{+:int}"] {
            assert!(
                matches!(
                    scan_template(src),
                    Err(ScanError::InvalidCaptureName { .. })
                ),
                "{src} must report an invalid capture name"
            );
        }

        // No colon at all is still an anonymous capture.
        let parts = scan_template("{int}").unwrap();
        match &parts[0] {
            TemplatePart::Capture { name, .. } => assert!(name.is_none()),
            other => panic!("expected an anonymous capture, got {other:?}"),
        }
    }

    /// **IP-05/IP-06.** Each capture keeps its **own** parser. The scanner used
    /// to throw the body away and leave a placeholder `Atomic { Int }` that the
    /// HIR tried to recover by rescanning the whole template and taking the
    /// first recognizable name — so `{name:word},{port:int}` gave both captures
    /// `word`. And a name it recognized nothing in defaulted to `Int`.
    #[test]
    fn every_capture_keeps_its_own_parser() {
        let parts = scan_template("{name:word},{port:int}").unwrap();
        assert_eq!(capture_kind(&parts[0]), AtomicKind::Word);
        assert_eq!(capture_kind(&parts[2]), AtomicKind::Int);

        // Anonymous captures too.
        let parts = scan_template("{word} {int}").unwrap();
        assert_eq!(capture_kind(&parts[0]), AtomicKind::Word);
        assert_eq!(capture_kind(&parts[2]), AtomicKind::Int);

        // There is no `Int` default: an unknown name is reported.
        assert!(matches!(
            scan_template("{value:intr}"),
            Err(ScanError::UnknownCaptureKind { .. })
        ));
        assert!(matches!(
            scan_template("{intr}"),
            Err(ScanError::UnknownCaptureKind { .. })
        ));
    }

    /// **D10's gate.** A capture body is a full parser expression, so the
    /// `}`-scan has to be brace-, paren- and string-aware. §7.7's own monkey
    /// example is `` `  Starting items: {items:csv(int)}` ``.
    #[test]
    fn a_capture_body_is_a_parser_expression() {
        // `parts[2]`: `"Starting items: "` is the literal `"Starting items:"`
        // and then the trailing run's own whitespace part (REP-20).
        let parts = scan_template("Starting items: {items:csv(int)}").unwrap();
        match &parts[2] {
            TemplatePart::Capture { parser, .. } => {
                assert!(matches!(parser.as_ref(), ParserAst::Csv { .. }));
            }
            other => panic!("expected a capture, got {other:?}"),
        }

        let parts = scan_template("{x:optional(int)}").unwrap();
        match &parts[0] {
            TemplatePart::Capture { parser, .. } => {
                assert!(matches!(parser.as_ref(), ParserAst::Optional { .. }));
            }
            other => panic!("expected a capture, got {other:?}"),
        }

        // A string argument, with a comma and a colon inside it — the extent
        // scan must not end the capture, and the name split must not fire.
        let parts = scan_template(r#"{s:sep("-", int)}"#).unwrap();
        match &parts[0] {
            TemplatePart::Capture { name, parser } => {
                assert_eq!(name.as_ref().map(|n| n.as_str()), Some("s"));
                match parser.as_ref() {
                    ParserAst::Sep { separator, .. } => assert_eq!(separator.as_str(), "-"),
                    other => panic!("expected Sep, got {other:?}"),
                }
            }
            other => panic!("expected a capture, got {other:?}"),
        }

        // A brace inside a string does **not** end the capture. The old scan to
        // the first `}` cut the body at the quote. Both braces are covered on
        // purpose: `"}"` happens to keep a brace *counter* balanced, so a
        // counter that ignores strings passes that case and fails this one —
        // which is exactly how the lexer's copy of the rule drifted.
        for (body, expect) in [
            (r#"{c:one_of("}")}"#, "}"),
            (r#"{c:one_of("{")}"#, "{"),
            (r#"{c:one_of("`")}"#, "`"),
        ] {
            let parts = scan_template(body).unwrap();
            match &parts[0] {
                TemplatePart::Capture { parser, .. } => match parser.as_ref() {
                    ParserAst::OneOf { chars, .. } => assert_eq!(chars, expect, "{body}"),
                    other => panic!("expected OneOf, got {other:?}"),
                },
                other => panic!("expected a capture, got {other:?}"),
            }
        }

        // A colon inside a nested call is not the name separator.
        let parts = scan_template("{g:choice(A: word, B: int)}").unwrap();
        match &parts[0] {
            TemplatePart::Capture { name, parser } => {
                assert_eq!(name.as_ref().map(|n| n.as_str()), Some("g"));
                match parser.as_ref() {
                    ParserAst::Choice { cases, .. } => assert_eq!(cases.len(), 2),
                    other => panic!("expected Choice, got {other:?}"),
                }
            }
            other => panic!("expected a capture, got {other:?}"),
        }

        // Malformed bodies report rather than being silently accepted.
        assert!(scan_template("{x:csv(int}").is_err());
        assert!(scan_template("{x:csv(int, int)}").is_err());
        assert!(scan_template("{x:frobnicate(int)}").is_err());
    }

    /// **A span is the text it names**, at every depth of nesting.
    ///
    /// `ParserAst::shift_spans` and `Span::shifted` are recursive span
    /// arithmetic that nothing asserted, and the one place the shift was *not*
    /// applied was the one D10 made reachable: a nested template's parts kept
    /// offsets relative to the **nested** interior and were never rebased onto
    /// the enclosing one, so `convert_template`'s single uniform shift — right
    /// for one level — left every caret under a nested template short by that
    /// interior's own offset.
    ///
    /// The assertion is the strongest available one: slice the interior by the
    /// span and compare it to the source text the node was built from.
    #[test]
    fn every_span_is_the_text_it_names_even_inside_a_nested_template() {
        fn text_at(interior: &str, span: praxis_source::Span) -> &str {
            &interior[span.start().to_usize()..span.end().to_usize()]
        }
        fn capture_parser(part: &TemplatePart) -> &ParserAst {
            match part {
                TemplatePart::Capture { parser, .. } => parser,
                other => panic!("expected a capture, got {other:?}"),
            }
        }

        // One level: the capture's parser span is the `int` that named it.
        // `parts[2]`: `"x = "` is `"x ="` plus the trailing run's own
        // whitespace part (REP-20).
        let interior = "x = {x:int}";
        let parts = scan_template(interior).unwrap();
        assert_eq!(
            text_at(interior, capture_parser(&parts[2]).span()),
            "int",
            "a top-level capture's span"
        );

        // Two levels. `int` lives inside the *nested* interior, and its span
        // must still name it in the text `scan_template` was handed.
        let interior = "{g:choice(A: `{x:int}`, B: word)}";
        let parts = scan_template(interior).unwrap();
        let ParserAst::Choice { cases, span } = capture_parser(&parts[0]) else {
            panic!("expected a choice");
        };
        assert_eq!(
            text_at(interior, *span),
            "choice(A: `{x:int}`, B: word)",
            "the choice call's own span"
        );
        let ParserAst::Template {
            parts: inner,
            span: inner_span,
        } = &cases[0].1
        else {
            panic!("expected a nested template");
        };
        assert_eq!(text_at(interior, *inner_span), "`{x:int}`");
        assert_eq!(
            text_at(interior, capture_parser(&inner[0]).span()),
            "int",
            "a capture inside a nested template — this is what was never rebased"
        );
        assert_eq!(
            text_at(interior, cases[1].1.span()),
            "word",
            "the un-nested sibling, which was always right"
        );

        // And the error channel is rebased too: the caret for a bad call inside
        // a nested template pointed at the *enclosing* call.
        let interior = "{g:choice(A: `{x:csv(int, int)}`)}";
        let err = scan_template(interior).unwrap_err();
        assert_eq!(
            err.byte_offset(),
            interior.find("csv").unwrap(),
            "the offset must name the `csv` that is wrong, not the `choice` around it"
        );
    }

    /// A compiler must not answer adversarial input with a stack overflow
    /// (D10). `scan_template` and `parse_capture_body` are mutually recursive
    /// now, so the bound is not optional.
    #[test]
    fn nesting_past_the_bound_is_an_error_and_not_a_stack_overflow() {
        let deep = format!("{}{}", "{a:".repeat(2_000), "}".repeat(2_000));
        assert!(
            matches!(scan_template(&deep), Err(ScanError::NestingTooDeep { .. })),
            "deep nesting must be refused before it recurses"
        );
        // The bound is far above what anyone writes: three levels is fine.
        assert!(scan_template("{a:optional(csv(int))}").is_ok());
    }

    /// The interior of a template nested `n` levels deep: `n = 1` is
    /// `{a:int}`, and each further level wraps the last in a capture holding a
    /// backtick template.
    fn nested_interior(n: usize) -> String {
        let mut interior = "{a:int}".to_string();
        for _ in 1..n {
            interior = format!("{{a:`{interior}`}}");
        }
        interior
    }

    /// **The number in the message is the number that is enforced, and it is
    /// the lexer's number.**
    ///
    /// Two layers bound template nesting: `praxis_syntax::template::template_end`
    /// decides how much text the lexer hands over, and the scanner's own
    /// recursion refuses before it can overflow the stack. The scanner's guard
    /// was incremented at *both* hops of the mutual recursion — once in
    /// `scan_template_at` for the capture body and again in `body::parse_expr`
    /// for the nested template — so one template level cost two and the
    /// scanner refused at **17** levels. The message still said 32.
    ///
    /// A stricter inner bound would have been defensible. A diagnostic naming a
    /// limit that nothing enforces is not, which is why this asserts the
    /// *rendered* number and not only the behaviour.
    #[test]
    fn the_two_template_nesting_bounds_are_the_same_number_and_the_message_says_it() {
        use praxis_syntax::template::{template_end, TemplateEnd};

        // The deepest nest the scanner accepts, measured rather than assumed.
        let deepest = (1..=MAX_NESTING + 4)
            .take_while(|n| scan_template(&nested_interior(*n)).is_ok())
            .last()
            .expect("one level at least");
        assert_eq!(
            deepest, MAX_NESTING,
            "the scanner's effective limit must be MAX_NESTING, not half of it"
        );

        // One past it refuses, and says so about *templates*.
        let err = scan_template(&nested_interior(MAX_NESTING + 1)).expect_err("one too deep");
        assert!(
            matches!(
                err,
                ScanError::NestingTooDeep {
                    what: "template",
                    ..
                }
            ),
            "the {}-level nest must be refused as template nesting, got {err}",
            MAX_NESTING + 1
        );
        let rendered = err.to_string();
        let named: usize = rendered
            .split_whitespace()
            .find_map(|w| w.parse().ok())
            .expect("the message names a limit");
        assert_eq!(
            named, deepest,
            "the message says {named} and the checker enforces {deepest}: {rendered}"
        );

        // And it is the lexer's number: `MAX_NESTING` *is*
        // `MAX_TEMPLATE_NESTING`, and at exactly that depth both layers take
        // the template whole — the lexer delivers one token spanning all of it
        // and the scanner reads it.
        //
        // Past the bound the two are not symmetric, and the comment should not
        // pretend otherwise: `template_end` stops treating a backtick as an
        // *opener* rather than refusing, so it still hands over a token. The
        // scanner is the layer that says no, which is why its number has to be
        // this one and its message has to name it.
        assert_eq!(MAX_NESTING, praxis_syntax::MAX_TEMPLATE_NESTING);
        let at_the_bound = format!("`{}`", nested_interior(MAX_NESTING));
        assert_eq!(
            template_end(&at_the_bound, 0),
            TemplateEnd::Closed(at_the_bound.len()),
            "the lexer delivers a {MAX_NESTING}-level template whole"
        );

        // A `(` bound is not a template bound, and the message no longer says
        // it is: `csv(` past the limit reported "template nesting is deeper
        // than 32" for text holding exactly one template.
        let parens = format!("{{a:{}int{}}}", "csv(".repeat(64), ")".repeat(64));
        let err = scan_template(&parens).expect_err("too many parens");
        assert!(
            matches!(err, ScanError::NestingTooDeep { what: "`(`", .. }),
            "a parenthesis bound must name parentheses, got {err}"
        );
    }
}
