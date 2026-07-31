//! The capture-body parser (§7.3, D10).
//!
//! A template capture's body is a **full parser expression**:
//! `{items:csv(int)}`, `{x:optional(int)}`, `{s:sep("-", int)}`,
//! `` {g:choice(A: `{n:int}`)} ``. §7.7's own monkey example writes the first
//! one, so "atomics only" is not a smaller language — it is a language that
//! cannot run the design document's text.
//!
//! Before this module the scanner **threw the body away** (IP-05). It stored a
//! placeholder `Atomic { Int }` and left a comment saying the HIR would
//! overwrite it; the HIR then rescanned the whole template from the beginning
//! and returned the *first* recognizable atomic name for every capture, so
//! `` `{name:word},{port:int}` `` typed both captures as `Text`. When it
//! recognized nothing at all it answered `Int` (IP-06), so `{value:intr}`
//! compiled.
//!
//! **This is a hand-written parser and not a call back into `praxis-parser`.**
//! ADR-023 fixes the dependency direction — `praxis-input-parser` must not
//! depend on the ordinary grammar — and the argument grammar is shared with the
//! HIR bridge through [`crate::call::build_call`] instead, so the two cannot
//! drift.

use praxis_source::Span;
use praxis_syntax::ident::{is_ident_continue, is_ident_start};

use crate::ast::{AtomicKind, Constructor, ParserAst};
use crate::call::{build_call, CallArg};
use crate::scan::{Scan, ScanError, MAX_NESTING};

/// Parse a capture body into its [`ParserAst`].
///
/// `at` is the body's byte offset within the text the caller is scanning, so
/// spans and error offsets are meaningful; `depth` is the current nesting depth
/// (see [`MAX_NESTING`]).
///
/// # Errors
/// [`ScanError`] for an unknown parser name, an unknown constructor, a
/// constructor whose arguments do not have §7.5's shape, or a body that is not
/// a parser expression at all.
pub(crate) fn parse_capture_body(
    text: &str,
    at: usize,
    depth: usize,
) -> Result<ParserAst, ScanError> {
    if depth > MAX_NESTING {
        return Err(ScanError::NestingTooDeep { byte_offset: at });
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(ScanError::EmptyCapture { byte_offset: at });
    }
    // The offset of `trimmed` within the caller's text, so errors point at the
    // body and not at its leading space.
    let base = at + (trimmed.as_ptr() as usize - text.as_ptr() as usize);
    let mut cur = Scan::new(trimmed);
    let ast = parse_expr(&mut cur, base, depth)?;
    skip_ws(&mut cur);
    if let Some((tail, _)) = cur.peek() {
        return Err(ScanError::MalformedCaptureBody {
            byte_offset: base + tail,
            message: format!("unexpected `{}` after the parser", &trimmed[tail..]),
        });
    }
    Ok(ast)
}

/// One parser expression: a nested template, a constructor call, or an atomic.
fn parse_expr(cur: &mut Scan<'_>, base: usize, depth: usize) -> Result<ParserAst, ScanError> {
    skip_ws(cur);
    let Some((start, c)) = cur.peek() else {
        return Err(ScanError::MalformedCaptureBody {
            byte_offset: base,
            message: "expected a parser".to_string(),
        });
    };

    if c == '`' {
        // A nested backtick template (D10). Its own interior is scanned by the
        // same scanner, one level deeper.
        let interior = crate::scan::take_template(cur)?;
        let parts = crate::scan::scan_template_at(interior, depth + 1)?;
        return Ok(ParserAst::Template {
            parts,
            span: Span::new((base + start) as u32, (base + cur.pos()) as u32),
        });
    }

    if !is_ident_start(c) {
        return Err(ScanError::MalformedCaptureBody {
            byte_offset: base + start,
            message: format!("`{c}` cannot begin a parser"),
        });
    }
    let name = take_ident(cur);
    skip_ws(cur);

    if cur.peek_char() != Some('(') {
        // A bare name: an atomic (§7.4). A constructor written without its
        // arguments is a shape error, not an unknown parser.
        if let Some(kind) = AtomicKind::from_keyword(name) {
            return Ok(ParserAst::Atomic {
                kind,
                span: Span::new((base + start) as u32, (base + cur.pos()) as u32),
            });
        }
        if Constructor::from_keyword(name).is_some() {
            return Err(ScanError::MalformedCaptureBody {
                byte_offset: base + start,
                message: format!("`{name}` is a constructor and needs arguments (§7.5)"),
            });
        }
        // **No `Int` default** (IP-06).
        return Err(ScanError::UnknownCaptureKind {
            byte_offset: base + start,
            name: name.to_string(),
        });
    }

    let Some(ctor) = Constructor::from_keyword(name) else {
        return Err(ScanError::UnknownConstructor {
            byte_offset: base + start,
            name: name.to_string(),
        });
    };
    let args = parse_args(cur, base, depth)?;
    let span = Span::new((base + start) as u32, (base + cur.pos()) as u32);
    build_call(ctor, args, span).map_err(|mut errs| {
        // `build_call` reports every problem; the scanner's channel carries one,
        // and the first is the one the source wrote first.
        ScanError::CallShape(errs.remove(0))
    })
}

/// The argument list of a constructor call. The cursor is on the `(`.
fn parse_args(cur: &mut Scan<'_>, base: usize, depth: usize) -> Result<Vec<CallArg>, ScanError> {
    let open = cur.pos();
    cur.bump(); // `(`
    let mut args = Vec::new();
    loop {
        skip_ws(cur);
        match cur.peek_char() {
            None => {
                return Err(ScanError::MalformedCaptureBody {
                    byte_offset: base + open,
                    message: "unbalanced `(`".to_string(),
                })
            }
            Some(')') => {
                cur.bump();
                return Ok(args);
            }
            Some(',') => {
                cur.bump();
            }
            Some(_) => args.push(parse_arg(cur, base, depth)?),
        }
    }
}

/// One argument: a string literal, a `name: value` pair, a bare flag, or a
/// positional parser.
fn parse_arg(cur: &mut Scan<'_>, base: usize, depth: usize) -> Result<CallArg, ScanError> {
    skip_ws(cur);
    if cur.peek_char() == Some('"') {
        return Ok(CallArg::String(take_string(cur, base)?));
    }

    // A `name:` prefix, if the next identifier is immediately followed by a
    // colon. Anything else is a positional parser.
    if let Some(name) = peek_named_prefix(cur) {
        for _ in 0..name.chars().count() {
            cur.bump();
        }
        skip_ws(cur);
        cur.bump(); // `:`
        skip_ws(cur);
        let name = name.to_string();

        // `skip:` and `fill:` take a keyword, not a parser.
        if name == "skip" || name == "fill" {
            let value = take_keyword_value(cur);
            return Ok(CallArg::Keyword { name, value });
        }
        // `name: repeated(P)` is the named-sections tail marker (§7.5): the
        // field's parser is the `P`, and `repeated` says it consumes every
        // remaining section rather than one. `build_call` refuses a bare
        // `repeated(...)` outright (IP-09), so the marker is unwrapped here
        // instead of going through it.
        if peek_ident(cur) == Some("repeated") {
            let at = cur.pos();
            take_ident(cur);
            skip_ws(cur);
            if cur.peek_char() != Some('(') {
                return Err(ScanError::MalformedCaptureBody {
                    byte_offset: base + at,
                    message: "`repeated` needs one parser argument (§7.5)".to_string(),
                });
            }
            let mut args = parse_args(cur, base, depth)?;
            if args.len() != 1 {
                return Err(ScanError::CallShape(crate::validate::ValidationError {
                    span: Span::at((base + at) as u32),
                    code: praxis_source::DiagCode::ConstructorArity,
                    message: format!("`repeated` expects 1 argument, got {}", args.len()),
                }));
            }
            let CallArg::Parser(parser) = args.remove(0) else {
                return Err(ScanError::CallShape(crate::validate::ValidationError {
                    span: Span::at((base + at) as u32),
                    code: praxis_source::DiagCode::InvalidConstructorArgument,
                    message: "`repeated`'s argument must be a parser (§7.5)".to_string(),
                }));
            };
            return Ok(CallArg::RepeatedTail { name, parser });
        }
        let parser = parse_expr(cur, base, depth)?;
        return Ok(CallArg::Named { name, parser });
    }

    // A bare flag — today only `grid(P, ragged, fill: v)`'s `ragged`.
    if peek_ident(cur) == Some("ragged") {
        take_ident(cur);
        return Ok(CallArg::Flag("ragged".to_string()));
    }

    Ok(CallArg::Parser(parse_expr(cur, base, depth)?))
}

/// Peek at `ident` `:` without consuming, returning the identifier text.
fn peek_named_prefix<'a>(cur: &mut Scan<'a>) -> Option<&'a str> {
    let name = peek_ident(cur)?;
    let src = cur.src();
    let start = cur.pos();
    let after = start + name.len();
    let rest = src.get(after..)?;
    let rest_trimmed = rest.trim_start();
    if rest_trimmed.starts_with(':') {
        Some(&src[start..after])
    } else {
        None
    }
}

/// The identifier at the cursor, without consuming it.
fn peek_ident<'a>(cur: &mut Scan<'a>) -> Option<&'a str> {
    let src = cur.src();
    let start = cur.pos();
    let rest = src.get(start..)?;
    let mut chars = rest.char_indices();
    let (_, first) = chars.next()?;
    if !is_ident_start(first) {
        return None;
    }
    let mut end = first.len_utf8();
    for (i, c) in chars {
        if is_ident_continue(c) {
            end = i + c.len_utf8();
        } else {
            break;
        }
    }
    Some(&rest[..end])
}

/// Consume and return the identifier at the cursor.
fn take_ident<'a>(cur: &mut Scan<'a>) -> &'a str {
    let start = cur.pos();
    while cur.peek_char().is_some_and(is_ident_continue) {
        cur.bump();
    }
    &cur.src()[start..cur.pos()]
}

/// Consume a `"…"` literal and decode it with the workspace's one decoder
/// (IP-08).
fn take_string(cur: &mut Scan<'_>, base: usize) -> Result<String, ScanError> {
    let start = cur.pos();
    cur.bump(); // opening quote
    loop {
        match cur.bump() {
            None => {
                return Err(ScanError::MalformedCaptureBody {
                    byte_offset: base + start,
                    message: "unterminated string literal".to_string(),
                })
            }
            Some((_, '\\')) => {
                cur.bump();
            }
            Some((_, '"')) => break,
            Some(_) => {}
        }
    }
    Ok(praxis_syntax::literal::unquote_text(
        &cur.src()[start..cur.pos()],
    ))
}

/// The value of a `skip:`/`fill:` keyword argument: everything up to the next
/// `,` or `)` at this level.
fn take_keyword_value(cur: &mut Scan<'_>) -> String {
    let start = cur.pos();
    while let Some(c) = cur.peek_char() {
        if c == ',' || c == ')' {
            break;
        }
        cur.bump();
    }
    cur.src()[start..cur.pos()].trim().to_string()
}

fn skip_ws(cur: &mut Scan<'_>) {
    while cur.peek_char().is_some_and(char::is_whitespace) {
        cur.bump();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{AtomicKind, SkipPolicy};

    fn parse(text: &str) -> Result<ParserAst, ScanError> {
        parse_capture_body(text, 0, 0)
    }

    #[test]
    fn an_atomic_body_is_the_atomic_it_names() {
        for name in ["int", "word", "char", "text", "rest", "digit"] {
            match parse(name) {
                Ok(ParserAst::Atomic { kind, .. }) => {
                    assert_eq!(kind, AtomicKind::from_keyword(name).unwrap());
                }
                other => panic!("{name} must be an atomic, got {other:?}"),
            }
        }
    }

    /// **IP-06.** There is no default: an unknown name is reported, and it is
    /// reported as `UnknownCaptureKind` (I012) rather than inheriting the
    /// generic template-scan code.
    #[test]
    fn an_unknown_parser_name_has_no_default() {
        match parse("intr") {
            Err(err @ ScanError::UnknownCaptureKind { .. }) => {
                assert_eq!(err.code(), praxis_source::DiagCode::UnknownCaptureKind);
            }
            other => panic!("expected UnknownCaptureKind, got {other:?}"),
        }
    }

    #[test]
    fn a_constructor_call_is_built_through_the_shared_table() {
        assert!(matches!(parse("csv(int)"), Ok(ParserAst::Csv { .. })));
        assert!(matches!(
            parse("optional(int)"),
            Ok(ParserAst::Optional { .. })
        ));
        match parse(r#"sep(",", int)"#) {
            Ok(ParserAst::Sep { separator, .. }) => assert_eq!(separator.as_str(), ","),
            other => panic!("expected Sep, got {other:?}"),
        }
        match parse(r#"chars(one_of("ab"), skip: newlines)"#) {
            Ok(ParserAst::Characters { skip, .. }) => assert_eq!(skip, SkipPolicy::Newlines),
            other => panic!("expected Characters, got {other:?}"),
        }
        // And the shape rules are the *same* rules, because they are the same
        // function: a wrong arity here reports exactly as it does at top level.
        assert!(matches!(
            parse("csv(int, int)"),
            Err(ScanError::CallShape(_))
        ));
        assert!(matches!(parse("choice(int)"), Err(ScanError::CallShape(_))));
    }

    #[test]
    fn an_unknown_constructor_is_reported_as_one() {
        match parse("frobnicate(int)") {
            Err(err @ ScanError::UnknownConstructor { .. }) => {
                assert_eq!(err.code(), praxis_source::DiagCode::UnknownConstructor);
            }
            other => panic!("expected UnknownConstructor, got {other:?}"),
        }
    }

    #[test]
    fn a_nested_template_is_a_parser_expression() {
        match parse("choice(A: `{n:int}`, B: word)") {
            Ok(ParserAst::Choice { cases, .. }) => {
                assert_eq!(cases.len(), 2);
                assert!(matches!(cases[0].1, ParserAst::Template { .. }));
            }
            other => panic!("expected Choice, got {other:?}"),
        }
    }

    /// The tail rules are §7.5's, and they are the same rules the top-level
    /// bridge applies — one `build_call` (IP-09).
    #[test]
    fn a_sections_tail_is_last_and_singular_here_too() {
        match parse("sections(draws: csv(int), boards: repeated(matrix(int)))") {
            Ok(ParserAst::SectionsNamed {
                fields,
                repeated_tail,
                ..
            }) => {
                assert_eq!(fields.len(), 1);
                let (name, tail) = repeated_tail.expect("a tail");
                assert_eq!(name, "boards");
                // The field's parser is the `P`, not the `repeated(P)` marker.
                assert!(matches!(*tail, ParserAst::Matrix { .. }));
            }
            other => panic!("expected SectionsNamed, got {other:?}"),
        }
        assert!(matches!(
            parse("sections(boards: repeated(int), draws: csv(int))"),
            Err(ScanError::CallShape(_))
        ));
        assert!(matches!(
            parse("sections(a: repeated(int), b: repeated(int))"),
            Err(ScanError::CallShape(_))
        ));
        assert!(matches!(
            parse("repeated(int)"),
            Err(ScanError::CallShape(_))
        ));
    }

    #[test]
    fn trailing_text_after_the_parser_is_an_error() {
        assert!(matches!(
            parse("int int"),
            Err(ScanError::MalformedCaptureBody { .. })
        ));
        assert!(matches!(
            parse("csv(int) x"),
            Err(ScanError::MalformedCaptureBody { .. })
        ));
    }
}
