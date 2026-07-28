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
    lower_to_plan, register_plan, scan_template, synthesize, validate, PlanId, ValidationError,
};
use praxis_source::{
    Diagnostic, DiagnosticCategory, DiagnosticCode, FileId, FileSpan, Severity, Span,
};
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

    let result_type = synthesize(&ast, db);
    // Registration is bounded and can refuse (IP-12). A refusal is a
    // diagnostic, not a wrapped index into somebody else's plan.
    let plan = match register_plan(lower_to_plan(&ast)) {
        Ok(id) => id,
        Err(e) => {
            diagnostics.push(err_diag(file, parser_expr.span(), "I001", e.to_string()));
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
    Some(synthesize(&ast, db))
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
                        "I010",
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
                "I000",
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
                "I030",
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

/// One argument to a constructor call. Most constructors take positional
/// parsers (`lines(int)`); `sep` takes a string separator; and the M9
/// constructors take named arguments (`rules: lines(...)`, `skip: whitespace`).
enum CallArg {
    /// A positional parser expression.
    Parser(ParserAst),
    /// A string literal (the separator for `sep`).
    String(String),
    /// A named argument `name: parser_expr` (M9, §7.5). `name` is the field/
    /// keyword; `parser` is the nested parser expression; `raw_text` is the
    /// source text of the value (used for keyword args like `skip: whitespace`
    /// whose value isn't a real parser expression).
    ///
    /// Consumed by the M9 constructors that take named/keyword args
    /// (`sections`, `block`, `chars`, `grid`, `optional`).
    Named {
        name: String,
        parser: ParserAst,
        raw_text: String,
    },
    /// The `repeated(...)` tail marker of named `sections`
    /// (`boards: repeated(matrix(int))`): `name` is the field, `parser`
    /// consumes every remaining section into a `Vec[result(P)]`.
    RepeatedTail { name: String, parser: ParserAst },
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
    let (ctor_name, args) = extract_call_args(&parser_call, file, diagnostics);

    // M9 constructors (§7.5) are dispatched by name before the M6
    // `Constructor::from_keyword` table — they have richer arg shapes (positional
    // + named) that the M6 table doesn't model.
    if ctor_name == "block" {
        return Some(build_block(args, span));
    }
    if ctor_name == "choice" {
        return Some(build_choice(args, span));
    }
    if ctor_name == "optional" {
        if let Some(CallArg::Parser(child)) = args.into_iter().next() {
            return Some(ParserAst::Optional {
                child: Box::new(child),
                span,
            });
        }
        return None;
    }
    if ctor_name == "scan" {
        if let Some(CallArg::Parser(child)) = args.into_iter().next() {
            return Some(ParserAst::Scan {
                child: Box::new(child),
                span,
            });
        }
        return None;
    }
    if ctor_name == "matrix" {
        if let Some(CallArg::Parser(child)) = args.into_iter().next() {
            return Some(ParserAst::Matrix {
                child: Box::new(child),
                span,
            });
        }
        return None;
    }
    if ctor_name == "one_of" {
        // one_of("LR") — one string-literal arg.
        if let Some(CallArg::String(s)) = args.into_iter().next() {
            return Some(ParserAst::OneOf { chars: s, span });
        }
        return None;
    }
    if ctor_name == "chars" {
        // chars(P, skip: policy). The positional parser is the cell parser; an
        // optional `skip:` named arg selects the skip policy.
        let mut iter = args.into_iter();
        let child = match iter.next() {
            Some(CallArg::Parser(p)) => p,
            _ => return None,
        };
        let mut skip = praxis_input_parser::SkipPolicy::Whitespace;
        for arg in iter {
            // `skip:` keyword arg — its value (none/whitespace/newlines) arrives
            // as raw text (it isn't a real parser expression).
            if let CallArg::Named {
                name,
                raw_text,
                parser: _,
            } = arg
            {
                if name == "skip" {
                    if let Some(policy) = praxis_input_parser::SkipPolicy::from_keyword(&raw_text) {
                        skip = policy;
                    }
                }
            }
        }
        return Some(ParserAst::Characters {
            child: Box::new(child),
            skip,
            span,
        });
    }
    if ctor_name == "grid" {
        // grid(P) is the M6 form; grid(P, ragged, fill: value) is the M9 ragged
        // form. Detect a `fill:` named arg.
        let is_ragged = args
            .iter()
            .any(|a| matches!(a, CallArg::Named { name, .. } if name == "fill"));
        if is_ragged {
            let mut child = None;
            let mut fill = String::new();
            for arg in args {
                match arg {
                    CallArg::Parser(p) if child.is_none() => child = Some(p),
                    CallArg::Named {
                        name,
                        raw_text,
                        parser: _,
                    } if name == "fill" => {
                        // The fill value's source text (e.g. "0" or ".").
                        fill = raw_text;
                    }
                    _ => {}
                }
            }
            if let Some(child) = child {
                return Some(ParserAst::GridRagged {
                    child: Box::new(child),
                    fill,
                    span,
                });
            }
            return None;
        }
        // Otherwise fall through to the M6 grid(P) handling below.
    }

    let ctor = Constructor::from_keyword(&ctor_name)?;

    // Arity check on positional args only, skipped for the heterogeneous
    // `sections` path (which legitimately has 0 positional args — all its args
    // are named). Named-arg shape is validated by `validate` on the resulting
    // `SectionsNamed` AST.
    let has_named_args = args
        .iter()
        .any(|a| matches!(a, CallArg::Named { .. } | CallArg::RepeatedTail { .. }));
    if !(ctor == Constructor::Sections && has_named_args) {
        let positional_arity = args
            .iter()
            .filter(|a| !matches!(a, CallArg::Named { .. } | CallArg::RepeatedTail { .. }))
            .count();
        if let Some(err) =
            praxis_input_parser::check_constructor_arity(ctor, positional_arity, span)
        {
            diagnostics.push(validation_error_to_diagnostic(&err, file));
        }
    }

    match ctor {
        Constructor::Lines
        | Constructor::Sections
        | Constructor::Csv
        | Constructor::Ws
        | Constructor::Grid => {
            // `sections` with named args is the heterogeneous form (M9, §7.5):
            // build a `SectionsNamed`. A named arg whose value is a `repeated(P)`
            // call is the tail and consumes all remaining sections.
            if ctor == Constructor::Sections
                && args.iter().any(|a| matches!(a, CallArg::Named { .. }))
            {
                return Some(build_sections_named(args, span));
            }
            if let Some(CallArg::Parser(child)) = args.into_iter().next() {
                Some(match ctor {
                    Constructor::Lines => ParserAst::Lines {
                        child: Box::new(child),
                        span,
                    },
                    Constructor::Sections => ParserAst::Sections {
                        child: Box::new(child),
                        span,
                    },
                    Constructor::Csv => ParserAst::Csv {
                        child: Box::new(child),
                        span,
                    },
                    Constructor::Ws => ParserAst::Ws {
                        child: Box::new(child),
                        span,
                    },
                    Constructor::Grid => ParserAst::Grid {
                        child: Box::new(child),
                        span,
                    },
                    Constructor::Sep => unreachable!(),
                })
            } else {
                None
            }
        }
        Constructor::Sep => {
            let mut iter = args.into_iter();
            let separator = match iter.next() {
                Some(CallArg::String(s)) => s,
                _ => String::new(),
            };
            match iter.next() {
                Some(CallArg::Parser(child)) => Some(ParserAst::Sep {
                    separator,
                    child: Box::new(child),
                    span,
                }),
                _ => None,
            }
        }
    }
}

/// Extract the constructor name and arguments from a `PARSER_CALL` rowan node.
fn extract_call_args(
    parser_call: &rowan::SyntaxNode<praxis_syntax::PraxisLanguage>,
    file: FileId,
    diagnostics: &mut Vec<Diagnostic>,
) -> (String, Vec<CallArg>) {
    let mut name = String::new();
    let mut args = Vec::new();

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
                                // The bare `ragged` flag in grid(P, ragged, fill:)
                                // is not a parser — skip it (the grid handler
                                // detects raggedness via the fill: arg).
                                let is_ragged_flag = pe.text().as_deref() == Some("ragged");
                                if is_ragged_flag {
                                    continue;
                                }
                                if let Some(converted) = convert_parser_expr(&pe, file, diagnostics)
                                {
                                    args.push(CallArg::Parser(converted));
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
                                    let is_keyword_value = name == "skip" || name == "fill";
                                    if is_keyword_value {
                                        args.push(CallArg::Named {
                                            name,
                                            parser: ParserAst::Atomic {
                                                kind: AtomicKind::Int,
                                                span: Span::at(0),
                                            },
                                            raw_text,
                                        });
                                        continue;
                                    }
                                    // `name: repeated(P)` is the named-sections
                                    // tail marker: consume all remaining sections.
                                    let is_repeated =
                                        value.constructor_name().as_deref() == Some("repeated");
                                    if is_repeated {
                                        if let Some(inner) = unwrap_repeated_child(&value) {
                                            if let Some(parser) =
                                                convert_parser_expr(&inner, file, diagnostics)
                                            {
                                                args.push(CallArg::RepeatedTail { name, parser });
                                            }
                                        }
                                    } else if let Some(parser) =
                                        convert_parser_expr(&value, file, diagnostics)
                                    {
                                        args.push(CallArg::Named {
                                            name,
                                            parser,
                                            raw_text,
                                        });
                                    } else if !raw_text.is_empty() {
                                        // The value didn't convert (e.g. a keyword
                                        // like `whitespace`); keep it as a raw
                                        // placeholder so keyword-arg handlers
                                        // (skip/fill) can still read it.
                                        args.push(CallArg::Named {
                                            name,
                                            parser: ParserAst::Atomic {
                                                kind: AtomicKind::Int,
                                                span: Span::at(0),
                                            },
                                            raw_text,
                                        });
                                    }
                                }
                            }
                        }
                        praxis_syntax::SyntaxKind::LITERAL => {
                            // A string-literal separator for `sep`.
                            let raw = arg.text().to_string();
                            let stripped = raw
                                .trim_start_matches('"')
                                .trim_end_matches('"')
                                .to_string();
                            args.push(CallArg::String(stripped));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    (name, args)
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
fn build_sections_named(args: Vec<CallArg>, span: Span) -> ParserAst {
    let mut fields: Vec<(String, ParserAst)> = Vec::new();
    let mut repeated_tail: Option<(String, Box<ParserAst>)> = None;
    for arg in args {
        match arg {
            CallArg::Named {
                name,
                parser,
                raw_text: _,
            } => fields.push((name, parser)),
            CallArg::RepeatedTail { name, parser } => {
                repeated_tail = Some((name, Box::new(parser)));
            }
            // A positional arg in a heterogeneous sections call is a structural
            // error; drop it (validation will have flagged the shape).
            _ => {}
        }
    }
    ParserAst::SectionsNamed {
        fields,
        repeated_tail,
        span,
    }
}

/// Build a `ParserAst::Block` from the args of a `block(item, ...)` call (M9,
/// §7.5). Positional parsers become `BlockItem::Positional` (a named-capture
/// template's fields flatten into the record; a scalar must be named or
/// validation rejects it); named args become `BlockItem::Named`.
fn build_block(args: Vec<CallArg>, span: Span) -> ParserAst {
    use praxis_input_parser::ast::BlockItem;
    let mut items = Vec::new();
    for arg in args {
        match arg {
            CallArg::Parser(p) => items.push(BlockItem::Positional(p)),
            CallArg::Named {
                name,
                parser,
                raw_text: _,
            } => {
                items.push(BlockItem::Named { name, parser });
            }
            // String / RepeatedTail args are not valid in a block; drop them
            // (validation surfaces the structural error).
            _ => {}
        }
    }
    ParserAst::Block { items, span }
}

/// Build a `ParserAst::Choice` from the args of a `choice(Name: P, ...)` call
/// (M9, §7.5). Each case is a named arg `(Name, P)`; positional / string args
/// are not valid in a choice and are dropped (validation surfaces the error).
fn build_choice(args: Vec<CallArg>, span: Span) -> ParserAst {
    let mut cases: Vec<(String, ParserAst)> = Vec::new();
    for arg in args {
        if let CallArg::Named {
            name,
            parser,
            raw_text: _,
        } = arg
        {
            cases.push((name, parser));
        }
    }
    ParserAst::Choice { cases, span }
}

// ---- diagnostic helpers ----------------------------------------------------

fn err_diag(file: FileId, span: Span, code: &str, msg: String) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagnosticCode::new(DiagnosticCategory::Input, code_number(code)),
        msg,
        FileSpan { file, span },
    )
}

/// Parse the numeric portion of a code like "I010" into a u32.
fn code_number(code: &str) -> u32 {
    code.trim_start_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .unwrap_or(0)
}

/// Convert a [`ValidationError`] into a [`Diagnostic`] (free function, avoids the
/// orphan rule since `ValidationError` lives in `praxis-input-parser`).
fn validation_error_to_diagnostic(err: &ValidationError, file: FileId) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagnosticCode::new(DiagnosticCategory::Input, code_number(err.code)),
        err.message.clone(),
        FileSpan {
            file,
            span: err.span,
        },
    )
}
