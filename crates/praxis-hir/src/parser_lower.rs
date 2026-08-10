//! Conversion of the rowan `ParserExpr` tree into the input-parser `ParserAst`
//! (§7.9).
//!
//! The ordinary language parser emits rowan nodes (`PARSER_EXPR`,
//! `PARSER_ATOM`, `PARSER_TEMPLATE`, `PARSER_CALL`, …). This module walks that
//! tree and builds a `praxis_input_parser::ParserAst`, which is then validated,
//! type-synthesized, and lowered to a `ParserPlan`.

use praxis_ast::{AstNode, ParserExpr, ParserExprKind, ParserNamedArg};
use praxis_input_parser::ast::{
    shift_part_spans, AtomicKind, Constructor, ParserAst, TemplatePart,
};
use praxis_input_parser::{
    build_call, build_repeated_tail, lower_to_plan, register_plan, scan_template, synthesize,
    synthesize_indexed, validate, CallArg, PlanId, ValidationError,
};

use crate::parser_index::ParserIndex;
use praxis_source::{DiagCode, Diagnostic, FileId, FileSpan, Severity, Span};
use praxis_types::{Type, TypeDb};

/// The result of converting + validating + synthesizing a parser expression.
pub struct ParserAnalysis {
    /// The compiled plan's id (for MIR to pass as an i64 immediate).
    pub plan: PlanId,
    /// The synthesized result type (for inference / hover).
    pub result_type: Type,
}

/// Analyze a `read`/`parse` body: convert the rowan tree to `ParserAst`,
/// validate it, synthesize its result type, lower it to a plan, and register
/// the plan. Returns the analysis on success, or pushes diagnostics on failure.
pub fn analyze_parser_expr(
    parser_expr: &ParserExpr,
    file: FileId,
    db: &mut TypeDb,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<ParserAnalysis> {
    let ast = convert_and_validate(parser_expr, file, diagnostics)?;

    let result_type = match synthesize(&ast, db) {
        Ok(ty) => ty,
        Err(e) => {
            report_conversion(diagnostics, file, parser_expr, e);
            return None;
        }
    };
    // Registration is bounded and can refuse. A refusal is a diagnostic, not a
    // wrapped index into somebody else's plan.
    // The arena decides the record layout order, because it is the only thing
    // that has seen every spelling of a shape in this program (ADR-152). It has
    // already seen this one: `synthesize` above registered a definition for it.
    let plan = match register_plan(lower_to_plan(&ast, db)) {
        Ok(id) => id,
        Err(e) => {
            report_conversion(diagnostics, file, parser_expr, e);
            return None;
        }
    };

    Some(ParserAnalysis { plan, result_type })
}

/// Synthesize only the result type of a parser expression (no plan
/// registration). Used during inference; lowering later does the full analysis.
///
/// **`index` is where the tree survives** (ADR-098). The converted AST and the
/// type of every node in it are appended here rather than dropped on the way out
/// — hover on an inner constructor, capture-type completion and the four parser
/// semantic-token classes all read this and nothing else. Nothing is appended
/// when conversion, validation or synthesis fails: an index entry for a tree the
/// compiler rejected would answer questions about a program that does not exist.
pub fn synthesize_parser_type(
    parser_expr: &ParserExpr,
    file: FileId,
    db: &mut TypeDb,
    diagnostics: &mut Vec<Diagnostic>,
    index: &mut Vec<ParserIndex>,
) -> Option<Type> {
    let ast = convert_and_validate(parser_expr, file, diagnostics)?;
    match synthesize_indexed(&ast, db) {
        Ok((ty, node_types)) => {
            index.push(ParserIndex {
                expr_range: parser_expr.syntax().text_range(),
                ast,
                node_types,
            });
            Some(ty)
        }
        Err(e) => {
            report_conversion(diagnostics, file, parser_expr, e);
            None
        }
    }
}

/// The prologue both entry points open with: convert the rowan tree, then
/// validate it, pushing every diagnostic on the way.
///
/// Shared rather than copied because **the two must reject the same trees for
/// the same reasons** — inference and lowering disagreeing about which programs
/// are well-formed is the silent divergence a fix applied to one copy only
/// would introduce.
fn convert_and_validate(
    parser_expr: &ParserExpr,
    file: FileId,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<ParserAst> {
    let ast = convert_parser_expr(parser_expr, file, diagnostics)?;
    let errs = validate(&ast);
    if !errs.is_empty() {
        for err in &errs {
            diagnostics.push(validation_error_to_diagnostic(err, file));
        }
        return None;
    }
    Some(ast)
}

/// Report a failure of the analysis that follows conversion — synthesis, or the
/// bounded plan registration — over the whole parser expression.
///
/// Those errors carry no span of their own: they are about the tree as a whole,
/// so the report underlines the whole of it.
fn report_conversion(
    diagnostics: &mut Vec<Diagnostic>,
    file: FileId,
    parser_expr: &ParserExpr,
    e: impl ToString,
) {
    diagnostics.push(err_diag(
        file,
        parser_expr.span(),
        DiagCode::ParserConversion,
        e.to_string(),
    ));
}

/// [`convert_parser_expr`] for tests, which need the `ParserAst` itself rather
/// than the type it synthesizes: a decoded separator and a capture's own parser
/// are not observable from the result type alone.
#[cfg(test)]
pub(crate) fn convert_parser_expr_for_test(
    parser_expr: &ParserExpr,
    file: FileId,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<ParserAst> {
    convert_parser_expr(parser_expr, file, diagnostics)
}

/// Convert a rowan `ParserExpr` into a `ParserAst`.
fn convert_parser_expr(
    parser_expr: &ParserExpr,
    file: FileId,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<ParserAst> {
    let span = parser_expr.span();
    match parser_expr.kind() {
        ParserExprKind::Atom => {
            let text = parser_expr.text().unwrap_or_default();
            match AtomicKind::from_keyword(&text) {
                Some(kind) => Some(ParserAst::Atomic { kind, span }),
                None => {
                    let diag = err_diag(
                        file,
                        span,
                        DiagCode::UnknownAtomic,
                        format!("unknown atomic parser `{text}`"),
                    );
                    // The atom node's span *is* the name, so the fix replaces it
                    // whole (ADR-132).
                    diagnostics.push(suggest_parser_name(diag, file, span, Some(text.as_str())));
                    None
                }
            }
        }
        ParserExprKind::Template => {
            let parts = convert_template(parser_expr, file, diagnostics);
            Some(ParserAst::Template { parts, span })
        }
        ParserExprKind::Call => convert_constructor_call(parser_expr, file, diagnostics, span),
        ParserExprKind::Unknown => {
            diagnostics.push(err_diag(
                file,
                span,
                DiagCode::MalformedParserExpression,
                "malformed parser expression".to_string(),
            ));
            None
        }
    }
}

/// Convert a template's `BacktickTemplate` token into `TemplatePart`s.
///
/// The scanner returns the parts **complete**, each capture carrying the parser
/// its own body names, so this function's job is to hand it the template's
/// interior and rebase the spans it hands back onto the file.
fn convert_template(
    parser_expr: &ParserExpr,
    file: FileId,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<TemplatePart> {
    let Some(token) = find_template_token(parser_expr) else {
        return Vec::new();
    };
    let text = token.text().to_string();
    // **An unterminated template has no interior**, so there is nothing to
    // scan. The lexer emits the token it managed to read, which for
    // `` read `{int` `` runs to the end of the file; scanning that text anyway
    // would answer with a second diagnostic describing something the source
    // does not contain, at a position that is not where anything is. The lexer
    // has already reported `T002` and it is the truthful report; there is
    // nothing to add.
    let Some(interior) = text.strip_prefix('`').and_then(|s| s.strip_suffix('`')) else {
        return Vec::new();
    };
    // The scanner works in offsets relative to the interior; the file offset of
    // the interior is the token's start plus the opening backtick.
    let base = u32::from(token.text_range().start()) + 1;

    match scan_template(interior) {
        // One uniform shift is right *here*, because the scanner's offsets are
        // all relative to this one interior — including those under a nested
        // template, which `body::parse_expr` has already rebased onto it.
        Ok(mut parts) => {
            shift_part_spans(&mut parts, base);
            parts
        }
        Err(e) => {
            // **The code comes from the error.** ADR-051 allocates a distinct
            // code for a bad capture name (I011), an unknown capture kind
            // (I012) and an unknown constructor (I013); flattening every
            // `ScanError` into `TemplateScan` (I030) here would leave those
            // three constructed nowhere in the tree. `ScanError::code` is an
            // exhaustive match, so a new variant has to decide.
            let at = base + e.byte_offset() as u32;
            let end = u32::from(token.text_range().end());
            // An error about a *name* underlines the name: the scanner anchors
            // at an offset, and the width comes from the word the error carries
            // as the one it could not resolve. Everything else keeps the point
            // it was anchored at, because for an unterminated capture or a
            // nesting bound there is no word to underline.
            let name = e.unknown_parser_name();
            let extent = match name {
                Some(name) => {
                    let stop = (at + name.len() as u32).min(end);
                    Span::new(at.min(stop), stop)
                }
                None => Span::at(at.min(end)),
            };
            let diag = err_diag(file, extent, e.code(), e.to_string());
            // A misspelled parser *inside* a capture is the same mistake as one
            // outside it, and gets the same fix (§15.3, ADR-132) — over the same
            // extent the report underlines.
            diagnostics.push(suggest_parser_name(diag, file, extent, name));
            Vec::new()
        }
    }
}

/// Find the `BacktickTemplate` token inside a `PARSER_TEMPLATE`.
///
/// The **token**, not its text: the capture bodies' spans are relative to the
/// template's interior and have to be rebased onto the file, which needs the
/// token's start.
fn find_template_token(parser_expr: &ParserExpr) -> Option<praxis_syntax::SyntaxToken> {
    use rowan::NodeOrToken;
    parser_expr
        .syntax()
        .descendants_with_tokens()
        .find_map(|child| match child {
            NodeOrToken::Token(t) if t.kind() == praxis_syntax::SyntaxKind::BacktickTemplate => {
                Some(t)
            }
            _ => None,
        })
}

/// Convert a constructor call rowan node into a `ParserAst`.
///
/// The **whole** of §7.5's argument handling lives in
/// [`praxis_input_parser::build_call`], which checks the call's shape before it
/// builds anything and which the capture-body parser shares. This function's
/// job is only to turn rowan into a [`CallArg`] list.
fn convert_constructor_call(
    parser_expr: &ParserExpr,
    file: FileId,
    diagnostics: &mut Vec<Diagnostic>,
    span: Span,
) -> Option<ParserAst> {
    // The PARSER_EXPR wraps a PARSER_CALL; descend to find it. A `?` here would
    // be a silent `None`: nothing downstream would report it.
    let Some(parser_call) = parser_expr
        .syntax()
        .children()
        .find(|c| c.kind() == praxis_syntax::SyntaxKind::PARSER_CALL)
    else {
        diagnostics.push(err_diag(
            file,
            span,
            DiagCode::MalformedParserExpression,
            "malformed parser constructor call".to_string(),
        ));
        return None;
    };
    let reported_before = diagnostics.len();
    let (ctor_name, args, all_args_converted) = extract_call_args(&parser_call, file, diagnostics);

    // An unknown constructor reports rather than answering a bare `None`: a
    // `?` on the `Option` would make `read frobnicate(int)` compile to nothing
    // with no diagnostic at all.
    let Some(ctor) = Constructor::from_keyword(&ctor_name) else {
        // The **name's** span, not the call's: it is what the report is about,
        // and it is what a fix replaces (§15.3, ADR-132). Falling back to the
        // call's span keeps the diagnostic total when the tree has no name node
        // to point at.
        let name_span = constructor_name_span(&parser_call).unwrap_or(span);
        let diag = err_diag(
            file,
            name_span,
            DiagCode::UnknownConstructor,
            format!("unknown parser constructor `{ctor_name}` (§7.5)"),
        );
        diagnostics.push(suggest_parser_name(
            diag,
            file,
            name_span,
            Some(ctor_name.as_str()),
        ));
        return None;
    };

    // An argument that did not convert has already reported; building on top of
    // the shortened list would report a *second*, wrong thing — an arity error
    // naming an argument the source did write.
    //
    // "Has already reported" is a claim about every path that clears the flag,
    // and this checks it rather than trusting it, because a silent `None` here
    // is invisible by construction.
    if !all_args_converted {
        if diagnostics.len() == reported_before {
            diagnostics.push(err_diag(
                file,
                span,
                DiagCode::InvalidConstructorArgument,
                format!("`{ctor_name}` has an argument that is not a parser expression (§7.5)"),
            ));
        }
        return None;
    }

    match build_call(ctor, args, span) {
        Ok(ast) => Some(ast),
        Err(errs) => {
            for err in &errs {
                diagnostics.push(validation_error_to_diagnostic(err, file));
            }
            None
        }
    }
}

/// Extract the constructor name and arguments from a `PARSER_CALL` rowan node.
///
/// The third element is whether **every** argument converted. A conversion
/// failure has already reported (an unknown atomic, a malformed expression);
/// building the shape check on a list that is short by one would then report a
/// second, wrong thing — an arity error naming an argument the source did
/// write.
fn extract_call_args(
    parser_call: &rowan::SyntaxNode<praxis_syntax::PraxisLanguage>,
    file: FileId,
    diagnostics: &mut Vec<Diagnostic>,
) -> (String, Vec<CallArg>, bool) {
    // The constructor's name comes first, on purpose: whether a `name:`
    // argument is a keyword (`chars`'s `skip:`, `grid`'s `fill:`) or a named
    // parser is **the constructor's** question, not one the argument's own name
    // can answer.
    let name = parser_call
        .children()
        .find(|c| c.kind() == praxis_syntax::SyntaxKind::PATH_EXPR)
        .map(|c| c.text().to_string())
        .unwrap_or_default();
    let ctor = Constructor::from_keyword(&name);
    let keyword_arg = ctor.and_then(Constructor::keyword_arg);
    let flag_arg = ctor.and_then(Constructor::flag_arg);
    let mut args = Vec::new();
    let mut all_converted = true;

    let Some(arg_list) = parser_call
        .children()
        .find(|c| c.kind() == praxis_syntax::SyntaxKind::PARSER_ARG_LIST)
    else {
        return (name, args, all_converted);
    };
    for arg in arg_list.children() {
        match arg.kind() {
            praxis_syntax::SyntaxKind::PARSER_EXPR | praxis_syntax::SyntaxKind::PARSER_TEMPLATE => {
                if let Some(pe) = praxis_ast::ParserExpr::cast(arg.clone()) {
                    // A bare keyword that is not a parser — today
                    // only `grid(P, ragged, fill: v)`'s `ragged`. It
                    // is carried as a `CallArg::Flag` so the shape
                    // table can require it.
                    //
                    // *Which* name that is is `Constructor::flag_arg`'s
                    // question, like `skip:`/`fill:` below. Read from
                    // the bare name alone, `ragged` would be a flag in
                    // **every** constructor's argument list, so
                    // `lines(ragged)` would be told it had written a
                    // flag where a parser belongs and the word would be
                    // reserved everywhere rather than in `grid`.
                    if let Some(flag) = flag_arg {
                        if pe.text().as_deref() == Some(flag) {
                            args.push(CallArg::Flag(flag.to_string()));
                            continue;
                        }
                    }
                    // **`repeated(P, n)` is a count that is not a literal, and
                    // that is what the reader needs to be told.** A bare name
                    // after `repeated`'s parser would otherwise be converted
                    // first, and an unknown one reports `I010 unknown atomic
                    // parser` — a true statement that carries none of the fix,
                    // because there is no name that would have worked. The
                    // plan is built when the program is compiled, so a count
                    // can only ever be written out. A name that *is* a parser
                    // (`repeated(P, word)`) still goes through the shape check
                    // below, which is where a second parser's arity is decided.
                    if ctor == Some(Constructor::Repeated)
                        && !args.is_empty()
                        && pe
                            .text()
                            .as_deref()
                            .is_some_and(|t| !praxis_input_parser::parser_names().any(|n| n == t))
                    {
                        diagnostics.push(err_diag(
                            file,
                            rowan_span(&arg),
                            DiagCode::InvalidConstructorArgument,
                            "`repeated`'s count must be a whole-number literal — the parser plan \
                             is built when the program is compiled, so the count cannot be a \
                             parser or a variable (§7.5)"
                                .to_string(),
                        ));
                        all_converted = false;
                        continue;
                    }
                    match convert_parser_expr(&pe, file, diagnostics) {
                        Some(converted) => args.push(CallArg::Parser(converted)),
                        None => all_converted = false,
                    }
                }
            }
            praxis_syntax::SyntaxKind::PARSER_NAMED_ARG => {
                // A named argument `name: parser_expr` (§7.5).
                if let Some(na) = ParserNamedArg::cast(arg.clone()) {
                    let Some(name) = na.name() else { continue };
                    // A **literal** value (`fill: 0`, `fill: "-"`): the grammar
                    // kept the token instead of failing to parse it as a parser
                    // expression. Only a constructor that has a keyword
                    // argument of this name can take one.
                    if let Some(literal) = na.keyword_value() {
                        if Some(name.as_str()) == keyword_arg {
                            args.push(CallArg::Keyword {
                                name,
                                value: literal,
                            });
                        } else {
                            diagnostics.push(err_diag(
                                file,
                                rowan_span(&arg),
                                DiagCode::InvalidConstructorArgument,
                                format!("`{name}:` takes a parser, not a literal value"),
                            ));
                            all_converted = false;
                        }
                        continue;
                    }
                    if let Some(value) = na.value() {
                        // Keyword args whose value isn't a real parser
                        // expression (skip:/fill:) are captured as raw
                        // text only — NOT converted, so no spurious
                        // "unknown atomic parser" diagnostic fires.
                        // Only for the constructor that has one: a
                        // `block` item or a `sections` field called
                        // `fill` is a field, not a keyword argument.
                        if Some(name.as_str()) == keyword_arg {
                            // **Not `unwrap_or_default`.** A value the AST
                            // cannot read is not the empty string; laundering
                            // it into one would pad a `fill: 0` grid with `""`
                            // and say nothing.
                            let Some(raw_text) = value.text() else {
                                diagnostics.push(err_diag(
                                    file,
                                    rowan_span(&arg),
                                    DiagCode::InvalidConstructorArgument,
                                    format!("`{name}:` needs a value (§7.5)"),
                                ));
                                all_converted = false;
                                continue;
                            };
                            args.push(CallArg::Keyword {
                                name,
                                value: raw_text,
                            });
                            continue;
                        }
                        // `name: repeated(P)` is the named-sections
                        // tail marker: consume all remaining sections.
                        let is_repeated = value.constructor_name().as_deref()
                            == Some(Constructor::Repeated.keyword());
                        if is_repeated {
                            // **The marker's whole argument list**,
                            // not its first parser child. The rule
                            // — exactly one argument, and it must
                            // be a parser — is §7.5's and lives in
                            // `build_repeated_tail`, which the
                            // capture-body front end calls too.
                            let tail_span = rowan_span(value.syntax());
                            match repeated_call_args(&value, file, diagnostics) {
                                Some(inner) => match build_repeated_tail(name, inner, tail_span) {
                                    Ok(tail) => args.push(tail),
                                    Err(errs) => {
                                        for err in &errs {
                                            diagnostics
                                                .push(validation_error_to_diagnostic(err, file));
                                        }
                                        all_converted = false;
                                    }
                                },
                                None => all_converted = false,
                            }
                        } else {
                            match convert_parser_expr(&value, file, diagnostics) {
                                Some(parser) => {
                                    args.push(CallArg::Named { name, parser });
                                }
                                None => all_converted = false,
                            }
                        }
                    }
                }
            }
            praxis_syntax::SyntaxKind::LITERAL => {
                // Two positional literals exist: a text one (`sep`'s separator,
                // `one_of`'s set) and a whole number (`repeated`'s count). The
                // decode is by the token's own kind rather than by which
                // constructor is being called, because which constructor takes
                // which is `check_call`'s question and asking it twice is how
                // two answers come to disagree.
                let raw = arg.text().to_string();
                if let Some(n) = parser_int_literal(&raw) {
                    args.push(CallArg::Int(n));
                } else {
                    match unquote_parser_literal(&raw) {
                        Some(text) => args.push(CallArg::String(text)),
                        None => {
                            diagnostics.push(err_diag(
                                file,
                                rowan_span(&arg),
                                DiagCode::InvalidConstructorArgument,
                                "a parser constructor's literal argument must be a text \
                                 literal or a whole number"
                                    .to_string(),
                            ));
                            all_converted = false;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    (name, args, all_converted)
}

/// Decode a parser constructor's whole-number argument — `repeated(P, N)`'s
/// count — or `None` if the token is not one.
///
/// The grammar puts an optional `-` and the digits in one `LITERAL` node, so
/// the text arriving here is what the source wrote. Decoding goes through
/// [`praxis_syntax::numeric::parse_int_literal`], the workspace's one integer
/// decoder, so `1_000` means here what it means everywhere else and a value
/// outside `Int` is `None` rather than a wrapped number.
fn parser_int_literal(raw: &str) -> Option<i64> {
    let trimmed = raw.trim();
    let digits = trimmed.strip_prefix('-').unwrap_or(trimmed);
    if digits.is_empty() || !digits.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    praxis_syntax::numeric::parse_int_literal(trimmed)
}

/// Decode a parser constructor's string-literal argument, or `None` if the
/// argument is not a well-formed text literal.
///
/// Delegates to [`praxis_syntax::literal::unquote_text`] — the workspace's one
/// decoder — so escapes mean here what they mean everywhere else: `sep("\t",
/// int)` splits on a tab rather than on the two characters `\` and `t`, and an
/// escaped quote survives instead of being trimmed off as a delimiter.
fn unquote_parser_literal(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if !praxis_syntax::literal::is_text_literal(trimmed) {
        return None;
    }
    Some(praxis_syntax::literal::unquote_text(trimmed))
}

/// The span of a rowan node, as a `praxis-source` [`Span`].
fn rowan_span(node: &rowan::SyntaxNode<praxis_syntax::PraxisLanguage>) -> Span {
    praxis_syntax::span_bridge::range_to_span(node.text_range())
}

/// The **whole** argument list of a `repeated(...)` tail marker, or `None` if
/// something in it did not convert (which has already reported).
///
/// The whole list, not the first parser child: the shape check — exactly one
/// argument, and it must be a parser — is [`build_repeated_tail`]'s, and it
/// needs the list it is checking. Taking the first child instead would lower
/// `repeated(matrix(int), word, int)` as `repeated(matrix(int))` with two
/// arguments silently gone, and answer `None` for `repeated()` with nothing
/// reported.
fn repeated_call_args(
    call: &ParserExpr,
    file: FileId,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Vec<CallArg>> {
    let Some(parser_call) = call
        .syntax()
        .children()
        .find(|c| c.kind() == praxis_syntax::SyntaxKind::PARSER_CALL)
    else {
        diagnostics.push(err_diag(
            file,
            rowan_span(call.syntax()),
            DiagCode::MalformedParserExpression,
            "malformed `repeated(...)` tail (§7.5)".to_string(),
        ));
        return None;
    };
    let (_name, args, all_converted) = extract_call_args(&parser_call, file, diagnostics);
    all_converted.then_some(args)
}

// ---- diagnostic helpers ----------------------------------------------------

fn err_diag(file: FileId, span: Span, code: DiagCode, msg: String) -> Diagnostic {
    Diagnostic::new(Severity::Error, code, msg, FileSpan { file, span })
}

/// Offer the nearest name in the parser table as a fix for `name`, over `span`.
///
/// The three unknown-name reports in this module — an atomic parser, a capture's
/// parser, a constructor — are one mistake against one closed table (§15.3,
/// ADR-132), so they ask [`praxis_input_parser::nearest_parser_name`] and
/// [`Diagnostic::with_did_you_mean`] the same question. What differs is *where*
/// the report underlines: the atom node, the capture's extent inside the
/// template, the constructor's name node. So the span is a parameter.
///
/// `name` is optional because a `ScanError` only sometimes knows which word it
/// could not resolve — for an unterminated capture or a nesting bound there is
/// no name to be near.
fn suggest_parser_name(
    diag: Diagnostic,
    file: FileId,
    span: Span,
    name: Option<&str>,
) -> Diagnostic {
    match name.and_then(praxis_input_parser::nearest_parser_name) {
        Some(near) => diag.with_did_you_mean(FileSpan { file, span }, near),
        None => diag,
    }
}

/// The source extent of a constructor call's name.
fn constructor_name_span(
    parser_call: &rowan::SyntaxNode<praxis_syntax::PraxisLanguage>,
) -> Option<Span> {
    let path = parser_call
        .children()
        .find(|c| c.kind() == praxis_syntax::SyntaxKind::PATH_EXPR)?;
    Some(praxis_syntax::span_bridge::range_to_span(path.text_range()))
}

/// Convert a [`ValidationError`] into a [`Diagnostic`] (free function, avoids the
/// orphan rule since `ValidationError` lives in `praxis-input-parser`).
fn validation_error_to_diagnostic(err: &ValidationError, file: FileId) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        err.code,
        err.message.clone(),
        FileSpan {
            file,
            span: err.span,
        },
    )
}
