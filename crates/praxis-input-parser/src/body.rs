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
use praxis_syntax::ident::{ident_run_len, is_ident_continue, is_ident_start};

use crate::ast::{shift_part_spans, AtomicKind, Constructor, ParserAst};
use crate::call::{build_call, build_repeated_tail, CallArg};
use crate::scan::{skip_string, Scan, ScanError};

/// Parse a capture body into its [`ParserAst`].
///
/// `at` is the body's byte offset within the text the caller is scanning, so
/// spans and error offsets are meaningful; `depth` is how many templates are
/// already open (see [`crate::scan::MAX_NESTING`]), threaded through to the backtick arm of
/// [`parse_expr`] and not re-checked here.
///
/// **The bound is checked in one place**, `scan_template_at`, because that is
/// the one place a template level is entered. This function used to check it
/// too, against a `depth` that its caller had already incremented — two guards
/// counting the same recursion twice, so the effective limit was half the one
/// the message named.
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
        // same scanner, one level deeper — and **in its own offsets**, which is
        // the whole of the next three lines.
        //
        // `scan_template_at` is handed the text between the backticks and knows
        // nothing about where that text sits here, so every span and every
        // error offset it returns is relative to the nested interior. The outer
        // `Template` node's span was rebased and the parts underneath it were
        // not, so a diagnostic inside `` `{a:sections(x: `{y:int}`)}` ``
        // pointed at the wrong bytes and `convert_template`'s single uniform
        // `shift_spans` could not repair it: that shift is right for one level
        // and wrong for two.
        //
        // The nested interior begins one byte past the backtick at `start`.
        let inner_base = base + start + 1;
        let interior = crate::scan::take_template(cur)?;
        let mut parts = crate::scan::scan_template_at(interior, depth + 1)
            .map_err(|e| e.shifted(inner_base))?;
        shift_part_spans(&mut parts, inner_base as u32);
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
    let args = parse_args(cur, base, depth, ctor)?;
    let span = Span::new((base + start) as u32, (base + cur.pos()) as u32);
    build_call(ctor, args, span).map_err(|mut errs| {
        // `build_call` reports every problem; the scanner's channel carries one,
        // and the first is the one the source wrote first.
        ScanError::CallShape(errs.remove(0))
    })
}

/// The argument list of `ctor(...)`. The cursor is on the `(`.
///
/// `ctor` is threaded through because whether a `name:` argument is a keyword
/// or a named parser is **the constructor's** question, not the name's.
fn parse_args(
    cur: &mut Scan<'_>,
    base: usize,
    depth: usize,
    ctor: Constructor,
) -> Result<Vec<CallArg>, ScanError> {
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
            Some(_) => {
                let at = args.len();
                args.push(parse_arg(cur, base, depth, ctor, at)?);
            }
        }
    }
}

/// One argument: a string literal, a whole number, a `name: value` pair, a bare
/// flag, or a positional parser.
///
/// `at` is the argument's position in the list, which only `repeated`'s count
/// needs: a name written where a count belongs has to be told it is not a
/// count, and by the time [`parse_expr`] has failed on it the diagnostic is
/// about the name instead.
fn parse_arg(
    cur: &mut Scan<'_>,
    base: usize,
    depth: usize,
    ctor: Constructor,
    at: usize,
) -> Result<CallArg, ScanError> {
    skip_ws(cur);
    if cur.peek_char() == Some('"') {
        return Ok(CallArg::String(take_string(cur, base)?));
    }

    // A positional whole number — `repeated(P, 6)`'s count. Which constructors
    // take one is `Constructor::arg_shape`'s question, not this scanner's, for
    // the same reason the ordinary grammar does not ask it either: a count
    // written where none belongs should be told so by name.
    if starts_a_number(cur) {
        let at = cur.pos();
        let text = take_number(cur);
        // `praxis_syntax::numeric` is the workspace's one integer decoder, so
        // the two front ends cannot disagree about what `0x10` or `1_000` mean.
        let Some(n) = praxis_syntax::numeric::parse_int_literal(text) else {
            return Err(ScanError::MalformedCaptureBody {
                byte_offset: base + at,
                message: format!("`{text}` is not a whole number"),
            });
        };
        return Ok(CallArg::Int(n));
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

        // `skip:` and `fill:` take a keyword, not a parser — but only for the
        // constructor that has one (`chars` and `grid`). Asking the *name*
        // instead of the constructor is what made a `block` item or a
        // `sections` field called `fill` into a keyword argument that the
        // builder then dropped.
        if Some(name.as_str()) == ctor.keyword_arg() {
            let value = take_keyword_value(cur);
            return Ok(CallArg::Keyword { name, value });
        }
        // `name: repeated(P)` / `name: repeated(P, N)` is the named-sections
        // group marker (§7.5): the field's parser is the `P`, and `repeated`
        // says the field takes a group of sections rather than one.
        // `build_call` refuses a bare `repeated(...)` outright (IP-09), so the
        // marker goes through `build_repeated_tail` — the same function the HIR
        // bridge calls, so the two front ends cannot disagree about the
        // marker's own shape, its count included.
        if peek_ident(cur) == Some(Constructor::Repeated.keyword()) {
            let at = cur.pos();
            take_ident(cur);
            skip_ws(cur);
            if cur.peek_char() != Some('(') {
                return Err(ScanError::MalformedCaptureBody {
                    byte_offset: base + at,
                    message: "`repeated` needs a parser argument (§7.5)".to_string(),
                });
            }
            let args = parse_args(cur, base, depth, Constructor::Repeated)?;
            return build_repeated_tail(name, args, Span::at((base + at) as u32))
                .map_err(|mut errs| ScanError::CallShape(errs.remove(0)));
        }
        let parser = parse_expr(cur, base, depth)?;
        return Ok(CallArg::Named { name, parser });
    }

    // A bare flag — today only `grid(P, ragged, fill: v)`'s `ragged`, and only
    // for the constructor that has one. Asked of the *name* alone, as this was,
    // `ragged` is a flag in **every** constructor's argument list: `lines(ragged)`
    // was told it had written a flag where a parser belongs, and the word was
    // reserved everywhere rather than in `grid`. That is `keyword_arg`'s bug one
    // argument kind over, and `flag_arg` is the same answer to it.
    //
    // `is_some_and` and not `peek_ident(cur) == ctor.flag_arg()`: that also
    // holds when both are `None`, which is end-of-arguments, and would mint a
    // flag out of nothing.
    if ctor.flag_arg().is_some_and(|f| peek_ident(cur) == Some(f)) {
        return Ok(CallArg::Flag(take_ident(cur).to_string()));
    }

    // A name after `repeated`'s parser is a count that is not a literal, and
    // the rowan front end says so there (ADR-073 Decision 2: one rule, two
    // spellings). Left to `parse_expr` this would report "unknown parser `n`",
    // which is true and carries none of the fix — no name would have worked.
    // A name that *is* a parser falls through to the shared shape check, which
    // is where a second parser is decided.
    if ctor == Constructor::Repeated && at >= 1 {
        if let Some(name) = peek_ident(cur) {
            if !crate::parser_names().any(|known| known == name) {
                return Err(ScanError::CallShape(crate::validate::ValidationError {
                    span: Span::at((base + cur.pos()) as u32),
                    code: praxis_source::DiagCode::InvalidConstructorArgument,
                    message: "`repeated`'s count must be a whole-number literal — the parser \
                              plan is built when the program is compiled, so the count cannot \
                              be a parser or a variable (§7.5)"
                        .to_string(),
                }));
            }
        }
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
    let rest = cur.src().get(cur.pos()..)?;
    match ident_run_len(rest) {
        0 => None,
        n => Some(&rest[..n]),
    }
}

/// Consume and return the identifier at the cursor.
fn take_ident<'a>(cur: &mut Scan<'a>) -> &'a str {
    let start = cur.pos();
    while cur.peek_char().is_some_and(is_ident_continue) {
        cur.bump();
    }
    &cur.src()[start..cur.pos()]
}

/// Whether a positional whole number starts at the cursor: a digit, or a `-`
/// with a digit behind it. A lone `-` is not a number, so it still reaches
/// [`parse_expr`] and earns that arm's diagnostic.
fn starts_a_number(cur: &mut Scan<'_>) -> bool {
    let rest = &cur.src()[cur.pos()..];
    let mut chars = rest.chars();
    match chars.next() {
        Some(c) if c.is_ascii_digit() => true,
        Some('-') => chars.next().is_some_and(|c| c.is_ascii_digit()),
        _ => false,
    }
}

/// Consume the number at the cursor: the sign, then the run of digits and
/// digit separators. Decoding it is [`praxis_syntax::numeric`]'s job, so a run
/// that is not a number is reported rather than reinterpreted.
fn take_number<'a>(cur: &mut Scan<'a>) -> &'a str {
    let start = cur.pos();
    if cur.peek_char() == Some('-') {
        cur.bump();
    }
    while cur
        .peek_char()
        .is_some_and(|c| c.is_ascii_digit() || c == '_')
    {
        cur.bump();
    }
    &cur.src()[start..cur.pos()]
}

/// Consume a `"…"` literal and decode it with the workspace's one decoder
/// (IP-08).
///
/// The **extent** is [`skip_string`]'s — [`praxis_syntax::template::string_end`]'s
/// — rather than the second copy of the backslash/quote loop this used to be.
/// The rebasing is not optional: this `Scan` runs over the capture body alone,
/// so the offset `skip_string` reports is relative to *that* text, and without
/// the shift every unterminated-literal caret in a capture body lands `base`
/// bytes short.
fn take_string(cur: &mut Scan<'_>, base: usize) -> Result<String, ScanError> {
    let start = cur.pos();
    skip_string(cur).map_err(|err| err.shifted(base))?;
    Ok(praxis_syntax::literal::unquote_text(
        &cur.src()[start..cur.pos()],
    ))
}

/// The value of a `skip:`/`fill:` keyword argument: everything up to the next
/// `,` or `)` **outside a string literal**.
///
/// The delimiter search used to be blind to quoting, so `fill: ","` ended at
/// the comma *inside* the literal and left a lone `"` behind — the scanner then
/// reported `unterminated string literal` for text that is not malformed, while
/// the rowan front end accepted the very same call. A quoted value is returned
/// with its quotes; `build_call` decodes it, so both front ends get one answer
/// from one place. Which literal is "a string" is [`skip_string`]'s question,
/// the same one the extent scan and the lexer ask, so a third inline copy of the
/// backslash/quote loop cannot drift away from them.
fn take_keyword_value(cur: &mut Scan<'_>) -> String {
    let start = cur.pos();
    while let Some(c) = cur.peek_char() {
        match c {
            ',' | ')' => break,
            '"' => {
                // A literal with no end has no end to skip to, and
                // `skip_string` reports that **without moving the cursor** — so
                // this arm must consume the rest itself, or the loop re-reads
                // this same quote forever. Running to the end is what the copy
                // this replaced did: `parse_args` then reports the unbalanced
                // `(`, which is the malformed text's real complaint.
                if skip_string(cur).is_err() {
                    cur.advance_to(cur.src().len());
                }
            }
            _ => {
                cur.bump();
            }
        }
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
    use crate::ast::{AtomicKind, SectionItem, SkipPolicy};

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

    /// **The counted form is the same one rule, in this front end too.** A
    /// bounded `repeated(P, N)` consumes exactly N sections, so the position
    /// argument the unbounded form rests on does not apply to it: something may
    /// follow it, and it may itself be last. The count's own refusals — zero,
    /// and a name where a literal belongs — are the shared builder's, so a
    /// capture body earns them without this scanner knowing what they are.
    #[test]
    fn a_counted_group_is_bounded_here_too() {
        match parse("sections(shapes: repeated(lines(int), 2), regions: lines(int))") {
            Ok(ParserAst::SectionsNamed {
                fields,
                repeated_tail,
                ..
            }) => {
                assert!(repeated_tail.is_none(), "a counted group is not the tail");
                assert_eq!(fields.len(), 2, "and something may follow it");
                match &fields[0] {
                    SectionItem::Counted {
                        name,
                        count,
                        parser,
                    } => {
                        assert_eq!(name, "shapes");
                        assert_eq!(count.get(), 2);
                        assert!(matches!(parser, ParserAst::Lines { .. }));
                    }
                    other => panic!("expected a counted group, got {other:?}"),
                }
                assert_eq!(fields[1].name(), "regions");
            }
            other => panic!("expected SectionsNamed, got {other:?}"),
        }

        // Last is also fine — "may appear anywhere" includes the end.
        assert!(matches!(
            parse("sections(regions: lines(int), shapes: repeated(lines(int), 2))"),
            Ok(ParserAst::SectionsNamed { .. })
        ));

        // A group of no sections parses nothing, and a count that is not a
        // literal cannot exist: the plan is built when the program is compiled.
        for refused in [
            "sections(a: repeated(int, 0))",
            "sections(a: repeated(int, -1))",
            "sections(a: repeated(int, word))",
            // A name that is not a parser either: this earns the count's own
            // diagnostic here, not "unknown parser `n`", which is the rowan
            // front end's answer too.
            "sections(a: repeated(int, n))",
            "sections(a: repeated(int, 2, 3))",
        ] {
            assert!(
                matches!(parse(refused), Err(ScanError::CallShape(_))),
                "`{refused}` must be refused by the shared shape check"
            );
        }
    }

    /// A `"…"` argument's extent is `scan::skip_string`'s, and the offset it
    /// reports is the body's own — so the caret has to be rebased onto the text
    /// the caller is scanning before it names a byte.
    #[test]
    fn an_unterminated_string_argument_is_reported_at_its_own_quote() {
        match parse_capture_body(r#"sep("-, int)"#, 10, 0) {
            Err(ScanError::MalformedCaptureBody {
                byte_offset,
                message,
            }) => {
                assert_eq!(byte_offset, 10 + 4, "the caret must be rebased by `at`");
                assert!(message.contains("unterminated string literal"), "{message}");
            }
            other => panic!("expected an unterminated literal, got {other:?}"),
        }
    }

    /// The same rule reaches a keyword argument's value, where a failure is not
    /// reported but *consumed*: the value runs to the end of the body. The point
    /// of the test is that this terminates at all — `skip_string` does not move
    /// the cursor when it fails, so an arm that only asked it to skip would loop
    /// on the opening quote.
    #[test]
    fn an_unterminated_keyword_value_ends_the_body_rather_than_looping() {
        assert!(matches!(
            parse(r#"chars(one_of("ab"), skip: "newlines)"#),
            Err(ScanError::MalformedCaptureBody { .. })
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
