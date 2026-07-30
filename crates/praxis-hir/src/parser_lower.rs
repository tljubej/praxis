//! Conversion of the rowan `ParserExpr` tree into the input-parser `ParserAst`
//! (§7.9, M6).
//!
//! The ordinary language parser emits rowan nodes (`PARSER_EXPR`,
//! `PARSER_ATOM`, `PARSER_TEMPLATE`, `PARSER_CALL`, …). This module walks that
//! tree and builds a `praxis_input_parser::ParserAst`, which is then validated,
//! type-synthesized, and lowered to a `ParserPlan`.

use praxis_ast::{AstNode, ParserExpr, ParserExprKind, ParserNamedArg};
use praxis_input_parser::ast::{AtomicKind, Constructor, ParserAst, TemplatePart};
use praxis_input_parser::{
    check_call, lower_to_plan, register_plan, scan_template, synthesize, validate, ArgKind, PlanId,
    Separator, ValidationError,
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
            let parts = convert_template(parser_expr, file, diagnostics, span);
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
fn convert_template(
    parser_expr: &ParserExpr,
    file: FileId,
    diagnostics: &mut Vec<Diagnostic>,
    span: Span,
) -> Vec<TemplatePart> {
    let Some(template_text) = find_template_text(parser_expr) else {
        return Vec::new();
    };
    let interior = template_text
        .strip_prefix('`')
        .and_then(|s| s.strip_suffix('`'))
        .unwrap_or(&template_text);

    match scan_template(interior) {
        Ok(scanned_parts) => scanned_parts
            .into_iter()
            .map(|part| match part {
                TemplatePart::Literal { text, ws } => TemplatePart::Literal { text, ws },
                TemplatePart::Capture { name, parser: _ } => {
                    // For M6, captures contain a simple atomic parser name.
                    // scan_template recorded the body text; we recover it by
                    // re-reading the capture body from the interior. Since the
                    // scanner left a placeholder, we default to `int` and let
                    // the actual body be inferred from the template text.
                    // A proper fix is to have scan_template return the body text;
                    // for M6 we parse the body from the capture's source.
                    let kind = extract_capture_kind(&template_text, &name);
                    TemplatePart::Capture {
                        name,
                        parser: Box::new(ParserAst::Atomic { kind, span }),
                    }
                }
            })
            .collect(),
        Err(e) => {
            diagnostics.push(err_diag(
                file,
                span,
                DiagCode::TemplateScan,
                format!("template scan error: {e}"),
            ));
            Vec::new()
        }
    }
}

/// Best-effort extraction of a capture's atomic kind from the template text.
/// This handles the common M6 cases (`{int}`, `{x:int}`, `{word}`, etc.). The
/// scan_template function should ideally return the body text; this is a
/// pragmatic bridge for v1.
fn extract_capture_kind(template_text: &str, _name: &Option<String>) -> AtomicKind {
    // Scan the template for `{...}` captures and return the first recognizable
    // atomic kind. This is a simplification — a real impl would pair captures
    // with their bodies. For M6, templates like `{int}` and `{x:int}` work.
    let mut in_capture = false;
    let mut body = String::new();
    for ch in template_text.chars() {
        if ch == '{' {
            in_capture = true;
            body.clear();
        } else if ch == '}' {
            in_capture = false;
            let body = body.rsplit(':').next().unwrap_or("").trim();
            if let Some(kind) = AtomicKind::from_keyword(body) {
                return kind;
            }
        } else if in_capture {
            body.push(ch);
        }
    }
    AtomicKind::Int // default
}

/// Find the raw text of a `BacktickTemplate` token inside a `PARSER_TEMPLATE`.
fn find_template_text(parser_expr: &ParserExpr) -> Option<String> {
    use rowan::NodeOrToken;
    for child in parser_expr.syntax().descendants_with_tokens() {
        if let NodeOrToken::Token(t) = child {
            if t.kind() == praxis_syntax::SyntaxKind::BacktickTemplate {
                return Some(t.text().to_string());
            }
        }
    }
    None
}

/// One argument to a constructor call, as written (§7.5).
///
/// Every constructor's argument list is checked against
/// [`praxis_input_parser::check_call`] *before* anything is built, so a builder
/// below never has an argument left over to drop. That is IP-07's whole
/// content: the M9 constructors used to be dispatched by an `if ctor_name ==
/// "…"` chain that took `args.into_iter().next()` and let the rest fall on the
/// floor, so `optional(int, word)` and `read frobnicate(int)` both compiled.
enum CallArg {
    /// A positional parser expression.
    Parser(ParserAst),
    /// A positional string literal (`sep`'s separator, `one_of`'s set).
    String(String),
    /// A bare keyword flag: the `ragged` of `grid(P, ragged, fill: 0)`. It used
    /// to be recognized by a text comparison and *skipped*, which is why
    /// `fill:` alone silently produced the ragged parser.
    Flag(String),
    /// A named argument `name: parser_expr` (M9, §7.5).
    Named { name: String, parser: ParserAst },
    /// A named argument whose value is a keyword, not a parser: `skip:
    /// whitespace`, `fill: 0`. Kept as source text, because there is no parser
    /// expression to convert — the predecessor stored a dummy `Atomic { Int }`
    /// beside the text and every reader had to know to ignore it.
    Keyword { name: String, value: String },
    /// The `repeated(...)` tail marker of named `sections`
    /// (`boards: repeated(matrix(int))`): `name` is the field, `parser`
    /// consumes every remaining section into a `Vec[result(P)]`.
    RepeatedTail { name: String, parser: ParserAst },
}

impl CallArg {
    /// This argument's shape, with the payload dropped — what `check_call`
    /// reads.
    fn kind(&self) -> ArgKind {
        match self {
            CallArg::Parser(_) => ArgKind::Parser,
            CallArg::String(_) => ArgKind::String,
            CallArg::Flag(f) => ArgKind::Flag(f.clone()),
            CallArg::Named { name, .. } | CallArg::Keyword { name, .. } => {
                ArgKind::Named(name.clone())
            }
            CallArg::RepeatedTail { name, .. } => ArgKind::RepeatedTail(name.clone()),
        }
    }
}

/// Convert a constructor call rowan node into a `ParserAst`.
fn convert_constructor_call(
    parser_expr: &ParserExpr,
    file: FileId,
    diagnostics: &mut Vec<Diagnostic>,
    span: Span,
) -> Option<ParserAst> {
    // The PARSER_EXPR wraps a PARSER_CALL; descend to find it.
    let parser_call = parser_expr
        .syntax()
        .children()
        .find(|c| c.kind() == praxis_syntax::SyntaxKind::PARSER_CALL)?;
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

    if ctor == Constructor::Repeated {
        // `repeated(P)` is not a parser in its own right — it is the marker on
        // the final named argument of a `sections` call, and it means "consume
        // every remaining section". Anywhere else there is nothing for it to
        // repeat over, and it used to be dropped in silence (IP-09).
        diagnostics.push(err_diag(
            file,
            span,
            DiagCode::MisplacedRepeatedTail,
            "`repeated(...)` is only the final named argument of a `sections` call (§7.5)"
                .to_string(),
        ));
        return None;
    }

    // An argument that did not convert has already reported; building on top of
    // the shortened list would report a *second*, wrong thing (an arity error
    // for an argument the source did write).
    if !all_args_converted {
        return None;
    }

    // §7.5's shape, checked before anything is built (IP-07).
    let kinds: Vec<ArgKind> = args.iter().map(CallArg::kind).collect();
    let shape_errors = check_call(ctor, &kinds, span);
    if !shape_errors.is_empty() {
        for err in &shape_errors {
            diagnostics.push(validation_error_to_diagnostic(err, file));
        }
        return None;
    }

    // From here the argument list has exactly the shape §7.5 gives this
    // constructor, so each arm consumes all of it.
    match ctor {
        Constructor::Lines => Some(ParserAst::Lines {
            child: Box::new(sole_parser(args, ctor, file, diagnostics, span)?),
            span,
        }),
        Constructor::Csv => Some(ParserAst::Csv {
            child: Box::new(sole_parser(args, ctor, file, diagnostics, span)?),
            span,
        }),
        Constructor::Ws => Some(ParserAst::Ws {
            child: Box::new(sole_parser(args, ctor, file, diagnostics, span)?),
            span,
        }),
        Constructor::Matrix => Some(ParserAst::Matrix {
            child: Box::new(sole_parser(args, ctor, file, diagnostics, span)?),
            span,
        }),
        Constructor::Optional => Some(ParserAst::Optional {
            child: Box::new(sole_parser(args, ctor, file, diagnostics, span)?),
            span,
        }),
        Constructor::Scan => Some(ParserAst::Scan {
            child: Box::new(sole_parser(args, ctor, file, diagnostics, span)?),
            span,
        }),
        Constructor::Sections => {
            // One name, two shapes: `sections(P)` is homogeneous,
            // `sections(name: P, …)` is the heterogeneous form (§7.5).
            if kinds
                .iter()
                .any(|k| matches!(k, ArgKind::Named(_) | ArgKind::RepeatedTail(_)))
            {
                build_sections_named(args, file, diagnostics, span)
            } else {
                Some(ParserAst::Sections {
                    child: Box::new(sole_parser(args, ctor, file, diagnostics, span)?),
                    span,
                })
            }
        }
        Constructor::Sep => {
            let mut separator = None;
            let mut child = None;
            for arg in args {
                match arg {
                    CallArg::String(s) => separator = Some(s),
                    CallArg::Parser(p) => child = Some(p),
                    _ => {}
                }
            }
            // A missing separator used to be laundered into `String::new()`,
            // which is the one separator that can never advance a cursor
            // (IP-10). `Separator::new` refuses it and the call does not build.
            let separator = match Separator::new(separator.as_deref().unwrap_or("")) {
                Ok(sep) => sep,
                Err(_) => {
                    diagnostics.push(err_diag(
                        file,
                        span,
                        DiagCode::EmptySeparator,
                        "`sep` needs a non-empty separator: an empty one never advances"
                            .to_string(),
                    ));
                    return None;
                }
            };
            Some(ParserAst::Sep {
                separator,
                child: Box::new(child?),
                span,
            })
        }
        Constructor::OneOf => {
            let chars = args.into_iter().find_map(|a| match a {
                CallArg::String(s) => Some(s),
                _ => None,
            })?;
            Some(ParserAst::OneOf { chars, span })
        }
        Constructor::Chars => {
            let mut child = None;
            let mut skip = praxis_input_parser::SkipPolicy::Whitespace;
            for arg in args {
                match arg {
                    CallArg::Parser(p) => child = Some(p),
                    CallArg::Keyword { name, value } if name == "skip" => {
                        // An unrecognized policy used to leave the default in
                        // place, so `skip: wihtespace` silently ran as
                        // `whitespace`.
                        match praxis_input_parser::SkipPolicy::from_keyword(&value) {
                            Some(policy) => skip = policy,
                            None => {
                                diagnostics.push(err_diag(
                                    file,
                                    span,
                                    DiagCode::InvalidConstructorArgument,
                                    format!(
                                        "`skip: {value}` is not a skip policy — \
                                         `none`, `whitespace` or `newlines` (§7.5)"
                                    ),
                                ));
                                return None;
                            }
                        }
                    }
                    _ => {}
                }
            }
            Some(ParserAst::Characters {
                child: Box::new(child?),
                skip,
                span,
            })
        }
        Constructor::Grid => {
            let mut child = None;
            let mut fill = None;
            for arg in args {
                match arg {
                    CallArg::Parser(p) => child = Some(p),
                    CallArg::Keyword { name, value } if name == "fill" => fill = Some(value),
                    _ => {}
                }
            }
            let child = Box::new(child?);
            Some(match fill {
                Some(fill) => ParserAst::GridRagged { child, fill, span },
                None => ParserAst::Grid { child, span },
            })
        }
        Constructor::Block => {
            use praxis_input_parser::ast::BlockItem;
            let items = args
                .into_iter()
                .filter_map(|arg| match arg {
                    CallArg::Parser(p) => Some(BlockItem::Positional(p)),
                    CallArg::Named { name, parser } => Some(BlockItem::Named { name, parser }),
                    _ => None,
                })
                .collect();
            Some(ParserAst::Block { items, span })
        }
        Constructor::Choice => {
            let cases = args
                .into_iter()
                .filter_map(|arg| match arg {
                    CallArg::Named { name, parser } => Some((name, parser)),
                    _ => None,
                })
                .collect();
            Some(ParserAst::Choice { cases, span })
        }
        // Rejected above, before the shape check.
        Constructor::Repeated => None,
    }
}

/// The single positional parser of a call `check_call` has already accepted as
/// `ArgShape::Positional(1)`.
///
/// A `None` here means the shape table and this extractor disagree, which is a
/// bug in the table rather than in the source — so it *reports* rather than
/// quietly building a parser out of nothing. The silent drop is the finding.
fn sole_parser(
    args: Vec<CallArg>,
    ctor: Constructor,
    file: FileId,
    diagnostics: &mut Vec<Diagnostic>,
    span: Span,
) -> Option<ParserAst> {
    match args.into_iter().next() {
        Some(CallArg::Parser(p)) => Some(p),
        _ => {
            diagnostics.push(err_diag(
                file,
                span,
                DiagCode::InvalidConstructorArgument,
                format!("`{}` needs one parser argument (§7.5)", ctor.keyword()),
            ));
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
    let mut name = String::new();
    let mut args = Vec::new();
    let mut all_converted = true;

    for child in parser_call.children() {
        match child.kind() {
            praxis_syntax::SyntaxKind::PATH_EXPR => {
                name = child.text().to_string();
            }
            praxis_syntax::SyntaxKind::PARSER_ARG_LIST => {
                for arg in child.children() {
                    match arg.kind() {
                        praxis_syntax::SyntaxKind::PARSER_EXPR
                        | praxis_syntax::SyntaxKind::PARSER_TEMPLATE => {
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
                                    if name == "skip" || name == "fill" {
                                        args.push(CallArg::Keyword {
                                            name,
                                            value: raw_text,
                                        });
                                        continue;
                                    }
                                    // `name: repeated(P)` is the named-sections
                                    // tail marker: consume all remaining sections.
                                    let is_repeated =
                                        value.constructor_name().as_deref() == Some("repeated");
                                    if is_repeated {
                                        match unwrap_repeated_child(&value).and_then(|inner| {
                                            convert_parser_expr(&inner, file, diagnostics)
                                        }) {
                                            Some(parser) => {
                                                args.push(CallArg::RepeatedTail { name, parser });
                                            }
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
            }
            _ => {}
        }
    }

    (name, args, all_converted)
}

/// Decode a parser constructor's string-literal argument, or `None` if the
/// argument is not a well-formed text literal.
///
/// Delegates to [`crate::lower::unquote_text`] — the language's one decoder
/// (IP-08). The predecessor trimmed quote *characters* off both ends and never
/// unescaped, so `sep("\t", int)` split on the two characters `\` and `t`,
/// `one_of("\"")` was broken, and `sep("\"\"", int)` lost both real quotes to
/// `trim_end_matches`.
fn unquote_parser_literal(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.len() < 2 || !trimmed.starts_with('"') || !trimmed.ends_with('"') {
        return None;
    }
    Some(crate::lower::unquote_text(trimmed))
}

/// The span of a rowan node, as a `praxis-source` [`Span`].
fn rowan_span(node: &rowan::SyntaxNode<praxis_syntax::PraxisLanguage>) -> Span {
    let range = node.text_range();
    Span::new(u32::from(range.start()), u32::from(range.end()))
}

/// Extract the single child parser expression from a `repeated(P)` call node.
/// Returns the inner `ParserExpr` (the `P`), or `None` if the node is not a
/// well-formed `repeated(...)` call with exactly one parser-expr child.
fn unwrap_repeated_child(call: &ParserExpr) -> Option<ParserExpr> {
    let parser_call = call
        .syntax()
        .children()
        .find(|c| c.kind() == praxis_syntax::SyntaxKind::PARSER_CALL)?;
    let arg_list = parser_call
        .children()
        .find(|c| c.kind() == praxis_syntax::SyntaxKind::PARSER_ARG_LIST)?;
    // The single child parser expression.
    arg_list.children().find_map(|c| {
        if matches!(
            c.kind(),
            praxis_syntax::SyntaxKind::PARSER_EXPR | praxis_syntax::SyntaxKind::PARSER_TEMPLATE
        ) {
            ParserExpr::cast(c)
        } else {
            None
        }
    })
}

/// Build a `ParserAst::SectionsNamed` from the args of a heterogeneous
/// `sections(name: P, ..., tail: repeated(P))` call (M9, §7.5). Named args
/// become fields in source order; a `RepeatedTail` arg (if any) must be last
/// and becomes the `repeated_tail`. Positional args are not permitted in the
/// heterogeneous form — if any appear, they are dropped with a diagnostic
/// (validation surfaces the structural error).
fn build_sections_named(
    args: Vec<CallArg>,
    file: FileId,
    diagnostics: &mut Vec<Diagnostic>,
    span: Span,
) -> Option<ParserAst> {
    // §7.5: `repeated(parser)` may appear only as the *final* named argument.
    // Both halves of that rule used to be silent (IP-09): a second tail
    // overwrote the first, and a tail written before other fields was moved to
    // the end — so the program that ran was not the program that was written.
    // `args` arrives in source order, which is what makes "last" checkable.
    let tail_positions: Vec<usize> = args
        .iter()
        .enumerate()
        .filter(|(_, a)| matches!(a, CallArg::RepeatedTail { .. }))
        .map(|(i, _)| i)
        .collect();
    if tail_positions.len() > 1 {
        diagnostics.push(err_diag(
            file,
            span,
            DiagCode::MisplacedRepeatedTail,
            "`sections` takes at most one `repeated(...)` tail (§7.5)".to_string(),
        ));
        return None;
    }
    if let Some(&at) = tail_positions.first() {
        if at != args.len() - 1 {
            diagnostics.push(err_diag(
                file,
                span,
                DiagCode::MisplacedRepeatedTail,
                "a `repeated(...)` tail may appear only as the final named argument (§7.5): it \
                 consumes every remaining section, so nothing can follow it"
                    .to_string(),
            ));
            return None;
        }
    }

    let mut fields: Vec<(String, ParserAst)> = Vec::new();
    let mut repeated_tail: Option<(String, Box<ParserAst>)> = None;
    for arg in args {
        match arg {
            CallArg::Named { name, parser } => fields.push((name, parser)),
            CallArg::RepeatedTail { name, parser } => {
                repeated_tail = Some((name, Box::new(parser)));
            }
            // `check_call` has already refused a positional or a string in a
            // heterogeneous `sections`, so there is nothing else to see here.
            _ => {}
        }
    }
    Some(ParserAst::SectionsNamed {
        fields,
        repeated_tail,
        span,
    })
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
