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
pub const MAX_NESTING: usize = 32;

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
    NestingTooDeep { byte_offset: usize },
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
            | ScanError::NestingTooDeep { byte_offset } => *byte_offset,
            ScanError::CallShape(err) => err.span.start().to_u32() as usize,
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
            ScanError::NestingTooDeep { byte_offset } => write!(
                f,
                "template nesting is deeper than {MAX_NESTING} at byte {byte_offset}"
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
pub(crate) fn scan_template_at(
    interior: &str,
    depth: usize,
) -> Result<Vec<TemplatePart>, ScanError> {
    if depth > MAX_NESTING {
        return Err(ScanError::NestingTooDeep { byte_offset: 0 });
    }
    let mut parts = Vec::new();
    let mut lit = String::new();
    let mut cur = Scan::new(interior);

    while let Some((at, c)) = cur.peek() {
        match c {
            '{' => {
                flush(&mut lit, WsPolicy::SpaceRun, &mut parts);
                let (name, body_text, body_at) = capture_extent(&mut cur, at)?;
                let name = match name {
                    Some(raw) => Some(capture_name(raw, at)?),
                    None => None,
                };
                let parser = crate::body::parse_capture_body(body_text, body_at, depth + 1)?;
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
                        flush(&mut lit, WsPolicy::SpaceRun, &mut parts);
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
    flush(&mut lit, WsPolicy::SpaceRun, &mut parts);
    Ok(parts)
}

/// Flush the accumulated literal run with its policy.
///
/// **A space/tab run at either end of a literal is flexible whitespace, not
/// text** (REP-20). §7.2's rule is that an ordinary run of spaces or tabs is
/// flexible, and both ends of a literal already have something that honours it:
/// the interpreter applies the literal's own policy before matching its bytes,
/// and every capture consumes leading whitespace before it walks. Leaving the
/// runs in the text made them *also* exact, so they had to be matched twice:
///
///   - The leading run could never match at all. §3.3's own
///     `` `{x1:int},{y1:int} -> {x2:int},{y2:int}` `` failed at the `-` of `->`
///     on every input, because the policy consumed the space and then the
///     literal " -> " was compared against "-> ".
///   - The trailing run made the spacing rigid on one side only: `1 -> 2`
///     matched and `1->2` did not.
///
/// A run *inside* a literal is untouched — nothing else consumes it, so it stays
/// exact — and `\x20` is the escape for a space that must be matched literally.
fn flush(lit: &mut String, ws: WsPolicy, parts: &mut Vec<TemplatePart>) {
    if lit.is_empty() {
        return;
    }
    let text = std::mem::take(lit);
    let stripped = text.trim_matches([' ', '\t']);
    // A literal whose runs strip it to nothing is pure whitespace, and it is
    // still emitted: the policy is the part.
    parts.push(TemplatePart::Literal {
        text: stripped.to_string(),
        ws,
    });
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
                take_template(cur, 0)?;
            }
            '\\' => {
                cur.bump();
                cur.bump();
            }
            '{' => {
                braces += 1;
                if braces > MAX_NESTING {
                    return Err(ScanError::NestingTooDeep { byte_offset: at });
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
                    return Err(ScanError::NestingTooDeep { byte_offset: at });
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
fn skip_string(cur: &mut Scan<'_>) -> Result<(), ScanError> {
    let open = cur.pos();
    cur.bump(); // the opening quote
    loop {
        match cur.bump() {
            None => {
                return Err(ScanError::MalformedCaptureBody {
                    byte_offset: open,
                    message: "unterminated string literal".to_string(),
                })
            }
            Some((_, '\\')) => {
                cur.bump();
            }
            Some((_, '"')) => return Ok(()),
            Some(_) => {}
        }
    }
}

/// Consume a nested `` `…` `` template run and return its **interior** (the
/// text between the backticks). The cursor is on the opening backtick.
///
/// A backtick inside a capture body opens a template of its own (D10), so
/// "close at the first backtick" is not the rule here: a backtick is a closer
/// only at brace depth zero, and one seen inside a capture opens a nested
/// template that this function consumes recursively.
pub(crate) fn take_template<'a>(cur: &mut Scan<'a>, depth: usize) -> Result<&'a str, ScanError> {
    if depth > MAX_NESTING {
        return Err(ScanError::NestingTooDeep {
            byte_offset: cur.pos(),
        });
    }
    let open = cur.pos();
    cur.bump(); // the opening backtick
    let interior_start = cur.pos();
    let mut braces = 0usize;
    loop {
        let Some((at, c)) = cur.peek() else {
            return Err(ScanError::MalformedCaptureBody {
                byte_offset: open,
                message: "unterminated nested template".to_string(),
            });
        };
        match c {
            '\\' => {
                cur.bump();
                cur.bump();
            }
            '{' => {
                braces += 1;
                if braces > MAX_NESTING {
                    return Err(ScanError::NestingTooDeep { byte_offset: at });
                }
                cur.bump();
            }
            '}' => {
                braces = braces.saturating_sub(1);
                cur.bump();
            }
            '`' => {
                if braces == 0 {
                    cur.bump();
                    return Ok(&cur.src()[interior_start..at]);
                }
                // A backtick inside a capture opens a template of its own.
                take_template(cur, depth + 1)?;
            }
            _ => {
                cur.bump();
            }
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
    /// becomes the whitespace policy and leaves the text.
    ///
    /// Here as well as in the JIT tests because this is the *scanner's* rule: the
    /// interpreter honours a policy it is given, and the defect was that the same
    /// spaces were also in the bytes it had to match.
    #[test]
    fn a_literals_edge_whitespace_is_its_policy_and_not_its_text() {
        // §3.3's own template. The middle literal is `-> `, not ` -> `.
        let parts = scan_template("{x1:int},{y1:int} -> {x2:int},{y2:int}").unwrap();
        assert_eq!(literals(&parts), vec![",", "->", ","]);

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

        // A literal that is only whitespace strips to nothing and is still emitted:
        // the policy is the part.
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
        let parts = scan_template("Starting items: {items:csv(int)}").unwrap();
        match &parts[1] {
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

        // A `}` inside a string does **not** end the capture. The old scan to
        // the first `}` cut the body at the quote.
        let parts = scan_template(r#"{c:one_of("}")}"#).unwrap();
        match &parts[0] {
            TemplatePart::Capture { parser, .. } => match parser.as_ref() {
                ParserAst::OneOf { chars, .. } => assert_eq!(chars, "}"),
                other => panic!("expected OneOf, got {other:?}"),
            },
            other => panic!("expected a capture, got {other:?}"),
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
}
