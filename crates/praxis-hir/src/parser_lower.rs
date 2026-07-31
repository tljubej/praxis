//! Conversion of the rowan `ParserExpr` tree into the input-parser `ParserAst`
//! (§7.9, M6).
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
    validate, CallArg, PlanId, ValidationError,
};
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
    let ast = convert_parser_expr(parser_expr, file, diagnostics)?;

    let errs = validate(&ast);
    if !errs.is_empty() {
        for err in &errs {
            diagnostics.push(validation_error_to_diagnostic(err, file));
        }
        return None;
    }

    let result_type = match synthesize(&ast, db) {
        Ok(ty) => ty,
        Err(e) => {
            diagnostics.push(err_diag(
                file,
                parser_expr.span(),
                DiagCode::ParserConversion,
                e.to_string(),
            ));
            return None;
        }
    };
    // Registration is bounded and can refuse (IP-12). A refusal is a
    // diagnostic, not a wrapped index into somebody else's plan.
    let plan = match register_plan(lower_to_plan(&ast)) {
        Ok(id) => id,
        Err(e) => {
            diagnostics.push(err_diag(
                file,
                parser_expr.span(),
                DiagCode::ParserConversion,
                e.to_string(),
            ));
            return None;
        }
    };

    Some(ParserAnalysis { plan, result_type })
}

/// Synthesize only the result type of a parser expression (no plan
/// registration). Used during inference; lowering later does the full analysis.
pub fn synthesize_parser_type(
    parser_expr: &ParserExpr,
    file: FileId,
    db: &mut TypeDb,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Type> {
    let ast = convert_parser_expr(parser_expr, file, diagnostics)?;
    let errs = validate(&ast);
    if !errs.is_empty() {
        for err in &errs {
            diagnostics.push(validation_error_to_diagnostic(err, file));
        }
        return None;
    }
    match synthesize(&ast, db) {
        Ok(ty) => Some(ty),
        Err(e) => {
            diagnostics.push(err_diag(
                file,
                parser_expr.span(),
                DiagCode::ParserConversion,
                e.to_string(),
            ));
            None
        }
    }
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
                    diagnostics.push(err_diag(
                        file,
                        span,
                        DiagCode::UnknownAtomic,
                        format!("unknown atomic parser `{text}`"),
                    ));
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
/// The scanner returns the parts **complete** now, each capture carrying the
/// parser its own body names (IP-05). This function used to throw that away and
/// rebuild every capture by calling `extract_capture_kind(&template_text,
/// &name)` — which rescanned the whole template from the beginning, returned
/// the *first* recognizable atomic name, and ignored the `name` parameter
/// entirely. So `` `{name:word},{port:int}` `` typed both captures `Text`, and
/// a template it recognized nothing in defaulted to `Int`.
fn convert_template(
    parser_expr: &ParserExpr,
    file: FileId,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<TemplatePart> {
    let Some(token) = find_template_token(parser_expr) else {
        return Vec::new();
    };
    let text = token.text().to_string();
    // **An unterminated template has no interior**, and scanning one anyway is
    // the IP-03 class: the lexer emits the token it managed to read, which for
    // `` read `{int` `` runs to the end of the file, and `unwrap_or(&text)` then
    // handed the scanner text whose offsets mean nothing. It answered with a
    // second diagnostic — "malformed capture body at byte 5: unterminated
    // nested template" — describing something the source does not contain, at a
    // position that is not where anything is. The lexer has already reported
    // `T002` and it is the truthful report; there is nothing to add.
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
            // **The code comes from the error** (IP-06). Every `ScanError` used
            // to be flattened into `TemplateScan` (I030) here, so the codes
            // ADR-051 allocated for a bad capture name (I011), an unknown
            // capture kind (I012) and an unknown constructor (I013) were
            // constructed nowhere in the tree. `ScanError::code` is an
            // exhaustive match, so a new variant has to decide.
            let at = base + e.byte_offset() as u32;
            diagnostics.push(err_diag(
                file,
                Span::at(at.min(u32::from(token.text_range().end()))),
                e.code(),
                e.to_string(),
            ));
            Vec::new()
        }
    }
}

/// Find the `BacktickTemplate` token inside a `PARSER_TEMPLATE`.
///
/// The **token**, not its text: the capture bodies' spans are relative to the
/// template's interior and have to be rebased onto the file, which needs the
/// token's start (IP-05).
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
/// job is only to turn rowan into a [`CallArg`] list. It used to be an
/// `if ctor_name == "…"` chain that ran ahead of the arity table, took
/// `args.into_iter().next()` and dropped the rest (IP-07).
fn convert_constructor_call(
    parser_expr: &ParserExpr,
    file: FileId,
    diagnostics: &mut Vec<Diagnostic>,
    span: Span,
) -> Option<ParserAst> {
    // The PARSER_EXPR wraps a PARSER_CALL; descend to find it. A `?` here was
    // one more silent `None`: nothing downstream would have reported it.
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

    // An unknown constructor used to be `Constructor::from_keyword(&name)?` —
    // a `?` on an `Option`, so `read frobnicate(int)` returned `None` with no
    // diagnostic at all and the whole `read` silently became nothing (IP-07).
    let Some(ctor) = Constructor::from_keyword(&ctor_name) else {
        diagnostics.push(err_diag(
            file,
            span,
            DiagCode::UnknownConstructor,
            format!("unknown parser constructor `{ctor_name}` (§7.5)"),
        ));
        return None;
    };

    // An argument that did not convert has already reported; building on top of
    // the shortened list would report a *second*, wrong thing — an arity error
    // naming an argument the source did write.
    //
    // "Has already reported" is an assumption, and it was **false**: the
    // `repeated(...)` unwrapper answered `None` for an empty argument list
    // without saying anything, so `sections(boards: repeated())` compiled to a
    // `read` that produced nothing, with zero diagnostics. Every path that
    // clears the flag now reports — and rather than trust that, this checks it,
    // because a silent `None` here is invisible by construction.
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
    // parser is **the constructor's** question, and this used to be answered
    // from the argument's name alone.
    let name = parser_call
        .children()
        .find(|c| c.kind() == praxis_syntax::SyntaxKind::PATH_EXPR)
        .map(|c| c.text().to_string())
        .unwrap_or_default();
    let keyword_arg = Constructor::from_keyword(&name).and_then(Constructor::keyword_arg);
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
                    // only `grid(P, ragged, fill: v)`'s `ragged`.
                    // It used to be *skipped*, so the shape table
                    // could not require it and a lone `fill:`
                    // silently produced the ragged parser.
                    if pe.text().as_deref() == Some("ragged") {
                        args.push(CallArg::Flag("ragged".to_string()));
                        continue;
                    }
                    match convert_parser_expr(&pe, file, diagnostics) {
                        Some(converted) => args.push(CallArg::Parser(converted)),
                        None => all_converted = false,
                    }
                }
            }
            praxis_syntax::SyntaxKind::PARSER_NAMED_ARG => {
                // A named argument `name: parser_expr` (M9, §7.5).
                if let Some(na) = ParserNamedArg::cast(arg.clone()) {
                    if let (Some(name), Some(value)) = (na.name(), na.value()) {
                        let raw_text = value.text().unwrap_or_default();
                        // Keyword args whose value isn't a real parser
                        // expression (skip:/fill:) are captured as raw
                        // text only — NOT converted, so no spurious
                        // "unknown atomic parser" diagnostic fires.
                        // Only for the constructor that has one: a
                        // `block` item or a `sections` field called
                        // `fill` is a field, and it used to be minted
                        // as a keyword and then dropped in silence.
                        if Some(name.as_str()) == keyword_arg {
                            args.push(CallArg::Keyword {
                                name,
                                value: raw_text,
                            });
                            continue;
                        }
                        // `name: repeated(P)` is the named-sections
                        // tail marker: consume all remaining sections.
                        let is_repeated = value.constructor_name().as_deref() == Some("repeated");
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
                match unquote_parser_literal(&arg.text().to_string()) {
                    Some(text) => args.push(CallArg::String(text)),
                    None => {
                        diagnostics.push(err_diag(
                            file,
                            rowan_span(&arg),
                            DiagCode::InvalidConstructorArgument,
                            "a parser constructor's literal argument must be a text \
                                         literal"
                                .to_string(),
                        ));
                        all_converted = false;
                    }
                }
            }
            _ => {}
        }
    }

    (name, args, all_converted)
}

/// Decode a parser constructor's string-literal argument, or `None` if the
/// argument is not a well-formed text literal.
///
/// Delegates to [`praxis_syntax::literal::unquote_text`] — the workspace's one
/// decoder (IP-08). The predecessor trimmed quote *characters* off both ends
/// and never unescaped, so `sep("\t", int)` split on the two characters `\` and
/// `t`, `one_of("\"")` was broken, and `sep("\"\"", int)` lost both real quotes
/// to `trim_end_matches`.
fn unquote_parser_literal(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if !praxis_syntax::literal::is_text_literal(trimmed) {
        return None;
    }
    Some(praxis_syntax::literal::unquote_text(trimmed))
}

/// The span of a rowan node, as a `praxis-source` [`Span`].
fn rowan_span(node: &rowan::SyntaxNode<praxis_syntax::PraxisLanguage>) -> Span {
    let range = node.text_range();
    Span::new(u32::from(range.start()), u32::from(range.end()))
}

/// The **whole** argument list of a `repeated(...)` tail marker, or `None` if
/// something in it did not convert (which has already reported).
///
/// This used to be `unwrap_repeated_child`: a `find_map` over the argument
/// list's children that returned the *first* parser expression and ignored
/// everything else. So `repeated(matrix(int), word, int)` lowered as
/// `repeated(matrix(int))` — two arguments silently gone — and `repeated()`
/// produced no diagnostic at all, because "no first child" was `None` and
/// `None` was assumed to have reported. The shape check is
/// [`build_repeated_tail`]'s, and it needs the list it is checking.
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
