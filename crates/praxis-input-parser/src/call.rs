//! Constructor calls: one argument list, one shape check, one builder (§7.5).
//!
//! Two callers construct a `sep(",", int)`: the HIR bridge, walking the rowan
//! tree of `read sep(",", int)`, and [`crate::body`], parsing the text of a
//! capture body `{xs:sep(",", int)}`. Before D10 was answered the second did
//! not exist and the first was an `if ctor_name == "…"` chain in `praxis-hir`
//! that dropped whatever it did not read (IP-07). Now they meet here: both
//! produce a [`CallArg`] list, and [`build_call`] is the only thing that turns
//! one into a [`ParserAst`]. Neither the shape rules nor the builders can drift
//! from each other, because there is one of each.

use crate::ast::{BlockItem, Constructor, ParserAst, Separator, SkipPolicy};
use crate::validate::{check_call, ArgKind, ValidationError};
use praxis_source::{DiagCode, Span};

/// One argument of a constructor call, as written (§7.5).
#[derive(Clone, Debug)]
pub enum CallArg {
    /// A positional parser expression.
    Parser(ParserAst),
    /// A positional string literal (`sep`'s separator, `one_of`'s set),
    /// **already decoded** by `praxis_syntax::literal::unquote_text`.
    String(String),
    /// A bare keyword flag: the `ragged` of `grid(P, ragged, fill: 0)`.
    Flag(String),
    /// A named argument `name: parser_expr`.
    Named { name: String, parser: ParserAst },
    /// A named argument whose value is a keyword rather than a parser:
    /// `skip: whitespace`, `fill: 0`.
    Keyword { name: String, value: String },
    /// The `repeated(...)` tail of a named `sections`.
    RepeatedTail { name: String, parser: ParserAst },
}

impl CallArg {
    /// This argument's shape, with the payload dropped — what [`check_call`]
    /// reads.
    #[must_use]
    pub fn kind(&self) -> ArgKind {
        match self {
            CallArg::Parser(_) => ArgKind::Parser,
            CallArg::String(_) => ArgKind::String,
            CallArg::Flag(f) => ArgKind::Flag(f.clone()),
            CallArg::Named { name, .. } => ArgKind::Named(name.clone()),
            // **Not `Named`.** These two collapsed onto one `ArgKind` and
            // `check_call` could not tell them apart, so `block`, `choice` and
            // named `sections` accepted a `skip:`/`fill:` keyword as a
            // well-shaped named argument and their builders then `filter_map`ed
            // it away: a field named `fill` vanished from the record with no
            // diagnostic.
            CallArg::Keyword { name, .. } => ArgKind::Keyword(name.clone()),
            CallArg::RepeatedTail { name, .. } => ArgKind::RepeatedTail(name.clone()),
        }
    }
}

/// Build the [`ParserAst`] for `ctor(args…)`, or report every reason it cannot
/// be built.
///
/// **Nothing is built before the shape is checked** (IP-07): the argument list
/// goes through [`check_call`] first, so by the time an arm below runs it has
/// exactly the arguments §7.5 gives that constructor and there is nothing left
/// for it to drop.
///
/// # Errors
/// A non-empty [`ValidationError`] list. Each carries the [`DiagCode`] the
/// caller reports it under.
pub fn build_call(
    ctor: Constructor,
    args: Vec<CallArg>,
    span: Span,
) -> Result<ParserAst, Vec<ValidationError>> {
    if ctor == Constructor::Repeated {
        // `repeated(P)` is not a parser in its own right — it is the marker on
        // the final named argument of a `sections` call, and it means "consume
        // every remaining section". Anywhere else there is nothing for it to
        // repeat over, and it used to be dropped in silence (IP-09).
        return Err(vec![ValidationError {
            span,
            code: DiagCode::MisplacedRepeatedTail,
            message: "`repeated(...)` is only the final named argument of a `sections` call (§7.5)"
                .to_string(),
        }]);
    }

    let kinds: Vec<ArgKind> = args.iter().map(CallArg::kind).collect();
    let shape_errors = check_call(ctor, &kinds, span);
    if !shape_errors.is_empty() {
        return Err(shape_errors);
    }

    let internal = |what: &str| {
        vec![ValidationError {
            span,
            code: DiagCode::InvalidConstructorArgument,
            message: format!("`{}` {what} (§7.5)", ctor.keyword()),
        }]
    };
    // **A `_ => {}` arm in a builder is how an argument disappears** (IP-07).
    // Every arm below is exhaustive over what its shape admits, and an argument
    // it does not know is *reported* rather than dropped. It should be
    // unreachable — `check_call` has already run — and the point is precisely
    // that if it ever is reachable, it is visible.
    let unexpected = |arg: &CallArg| {
        vec![ValidationError {
            span,
            code: DiagCode::InvalidConstructorArgument,
            message: format!(
                "`{}` does not take {} (§7.5)",
                ctor.keyword(),
                arg.kind().describe()
            ),
        }]
    };

    match ctor {
        Constructor::Lines
        | Constructor::Csv
        | Constructor::Ws
        | Constructor::Matrix
        | Constructor::Optional
        | Constructor::Scan => {
            let child = Box::new(sole_parser(args).ok_or_else(|| internal("needs one parser"))?);
            Ok(match ctor {
                Constructor::Lines => ParserAst::Lines { child, span },
                Constructor::Csv => ParserAst::Csv { child, span },
                Constructor::Ws => ParserAst::Ws { child, span },
                Constructor::Matrix => ParserAst::Matrix { child, span },
                Constructor::Optional => ParserAst::Optional { child, span },
                _ => ParserAst::Scan { child, span },
            })
        }
        Constructor::Sections => {
            // One name, two shapes: `sections(P)` is homogeneous,
            // `sections(name: P, …)` is the heterogeneous form.
            if kinds
                .iter()
                .any(|k| matches!(k, ArgKind::Named(_) | ArgKind::RepeatedTail(_)))
            {
                build_sections_named(args, span)
            } else {
                Ok(ParserAst::Sections {
                    child: Box::new(sole_parser(args).ok_or_else(|| internal("needs one parser"))?),
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
                    other => return Err(unexpected(&other)),
                }
            }
            // A missing separator used to be laundered into `String::new()` by
            // the HIR bridge, which is the one separator that can never advance
            // a cursor (IP-10). `Separator::new` refuses it.
            let separator = Separator::new(separator.as_deref().unwrap_or("")).map_err(|_| {
                vec![ValidationError {
                    span,
                    code: DiagCode::EmptySeparator,
                    message: "`sep` needs a non-empty separator: an empty one never advances"
                        .to_string(),
                }]
            })?;
            Ok(ParserAst::Sep {
                separator,
                child: Box::new(child.ok_or_else(|| internal("needs an element parser"))?),
                span,
            })
        }
        Constructor::OneOf => {
            let mut chars = None;
            for arg in args {
                match arg {
                    CallArg::String(s) => chars = Some(s),
                    other => return Err(unexpected(&other)),
                }
            }
            Ok(ParserAst::OneOf {
                chars: chars.ok_or_else(|| internal("needs a character set"))?,
                span,
            })
        }
        Constructor::Chars => {
            let mut child = None;
            let mut skip = SkipPolicy::Whitespace;
            for arg in args {
                match arg {
                    CallArg::Parser(p) => child = Some(p),
                    CallArg::Keyword { name, value } if name == "skip" => {
                        // An unrecognized policy used to leave the default in
                        // place, so `skip: wihtespace` silently ran as
                        // `whitespace`.
                        skip = SkipPolicy::from_keyword(&value).ok_or_else(|| {
                            vec![ValidationError {
                                span,
                                code: DiagCode::InvalidConstructorArgument,
                                // The three names alone are a trap: nothing in
                                // them says `newlines` is the *broader* policy.
                                // Each one states what it skips.
                                message: format!(
                                    "`skip: {value}` is not a skip policy — `none` (skips {}), \
                                     `whitespace` (skips {}) or `newlines` (skips {}) (§7.5)",
                                    SkipPolicy::None.skips(),
                                    SkipPolicy::Whitespace.skips(),
                                    SkipPolicy::Newlines.skips(),
                                ),
                            }]
                        })?;
                    }
                    other => return Err(unexpected(&other)),
                }
            }
            Ok(ParserAst::Characters {
                child: Box::new(child.ok_or_else(|| internal("needs a character parser"))?),
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
                    CallArg::Keyword { name, value } if name == "fill" => {
                        // The decode lives here, once, so both front ends
                        // agree: `fill: "-"` reached the plan as `"\"-\""`,
                        // quotes and all, because the value was carried as raw
                        // text and nothing ever unquoted it (IP-08's rule for
                        // every other parser string literal).
                        let decoded = praxis_syntax::literal::unquote_text(&value);
                        // **A keyword argument's value is part of its shape.**
                        // `chars`'s `skip:` has always checked its value; this
                        // one checked nothing, so `grid(P, ragged, fill:)` was
                        // accepted with no diagnostic at all and built a ragged
                        // grid padded with the empty string. That is the same
                        // unrepresentable value IP-10 refuses one field over,
                        // where `Separator::new` rejects an empty separator
                        // because it never advances: a cell of no characters
                        // pads nothing.
                        if decoded.is_empty() {
                            return Err(vec![ValidationError {
                                span,
                                code: DiagCode::InvalidConstructorArgument,
                                message: "`fill:` needs a value to pad a short row with — an \
                                          empty one fills nothing (§7.5)"
                                    .to_string(),
                            }]);
                        }
                        fill = Some(decoded);
                    }
                    // `ragged` carries nothing: it exists so the shape table
                    // can *require* it beside `fill:`. Named here rather than
                    // swept up by a wildcard, so it is a decision and not a
                    // leak.
                    CallArg::Flag(f) if f == "ragged" => {}
                    other => return Err(unexpected(&other)),
                }
            }
            let child = Box::new(child.ok_or_else(|| internal("needs a cell parser"))?);
            Ok(match fill {
                Some(fill) => ParserAst::GridRagged { child, fill, span },
                None => ParserAst::Grid { child, span },
            })
        }
        Constructor::Block => {
            let mut items = Vec::with_capacity(args.len());
            for arg in args {
                match arg {
                    CallArg::Parser(p) => items.push(BlockItem::Positional(p)),
                    CallArg::Named { name, parser } => {
                        items.push(BlockItem::Named { name, parser })
                    }
                    other => return Err(unexpected(&other)),
                }
            }
            Ok(ParserAst::Block { items, span })
        }
        Constructor::Choice => {
            let mut cases = Vec::with_capacity(args.len());
            for arg in args {
                match arg {
                    CallArg::Named { name, parser } => cases.push((name, parser)),
                    other => return Err(unexpected(&other)),
                }
            }
            Ok(ParserAst::Choice { cases, span })
        }
        // Refused at the top, before the shape check.
        Constructor::Repeated => Err(internal("is not a parser")),
    }
}

/// Build the `name: repeated(P)` tail marker of a named `sections` (§7.5).
///
/// `repeated(P)` is not a parser in its own right — it is the marker on the
/// final named argument of a `sections` call, and the field's parser is the
/// `P`. §7.5 gives the marker exactly one argument, and that argument must be a
/// parser. That is the whole rule, and it lives here for the same reason
/// [`build_call`] does: **both front ends must apply it**.
///
/// They did not. The capture-body parser checked it inline; the HIR bridge
/// unwrapped the marker with a `find_map` that returned the *first* parser-expr
/// child of the argument list and ignored the rest. So
/// `repeated(matrix(int), word, int)` quietly became `repeated(matrix(int))`
/// and `repeated()` produced no diagnostic at all — while the identical text
/// written inside a capture body was rejected with I022. ADR-073 claims the two
/// front ends share one shape check; this is the function that makes the claim
/// true for the tail marker.
///
/// # Errors
/// A non-empty [`ValidationError`] list: `ConstructorArity` (I022) for a wrong
/// count, `InvalidConstructorArgument` (I014) for a wrong kind.
pub fn build_repeated_tail(
    name: String,
    mut args: Vec<CallArg>,
    span: Span,
) -> Result<CallArg, Vec<ValidationError>> {
    if args.len() != 1 {
        return Err(vec![ValidationError {
            span,
            code: DiagCode::ConstructorArity,
            message: format!("`repeated` expects 1 argument, got {}", args.len()),
        }]);
    }
    match args.remove(0) {
        CallArg::Parser(parser) => Ok(CallArg::RepeatedTail { name, parser }),
        other => Err(vec![ValidationError {
            span,
            code: DiagCode::InvalidConstructorArgument,
            message: format!(
                "`repeated`'s argument must be a parser, but it is {} (§7.5)",
                other.kind().describe()
            ),
        }]),
    }
}

/// The single positional parser of a call `check_call` has already accepted.
fn sole_parser(args: Vec<CallArg>) -> Option<ParserAst> {
    match args.into_iter().next() {
        Some(CallArg::Parser(p)) => Some(p),
        _ => None,
    }
}

/// Build a heterogeneous `sections(name: P, …, tail: repeated(P))` (§7.5).
///
/// §7.5: "`repeated(parser)` may appear only as the final named argument."
/// Neither half of that was checked (IP-09) — a second tail silently overwrote
/// the first, and a tail written before another field was silently moved to the
/// end, so the parser that ran was not the one that was written. `args` is in
/// source order, which is what makes "final" a checkable claim.
fn build_sections_named(args: Vec<CallArg>, span: Span) -> Result<ParserAst, Vec<ValidationError>> {
    let tails: Vec<usize> = args
        .iter()
        .enumerate()
        .filter(|(_, a)| matches!(a, CallArg::RepeatedTail { .. }))
        .map(|(i, _)| i)
        .collect();
    if tails.len() > 1 {
        return Err(vec![ValidationError {
            span,
            code: DiagCode::MisplacedRepeatedTail,
            message: "`sections` takes at most one `repeated(...)` tail (§7.5)".to_string(),
        }]);
    }
    if let Some(&at) = tails.first() {
        if at != args.len() - 1 {
            return Err(vec![ValidationError {
                span,
                code: DiagCode::MisplacedRepeatedTail,
                message: "a `repeated(...)` tail may appear only as the final named argument \
                          (§7.5): it consumes every remaining section, so nothing can follow it"
                    .to_string(),
            }]);
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
            // `check_call` has already refused a positional, a string or a
            // keyword here — and this reports rather than drops, because a
            // `_ => {}` is how a field vanishes from a record in silence.
            other => {
                return Err(vec![ValidationError {
                    span,
                    code: DiagCode::InvalidConstructorArgument,
                    message: format!(
                        "`sections` does not take {} (§7.5)",
                        other.kind().describe()
                    ),
                }])
            }
        }
    }
    Ok(ParserAst::SectionsNamed {
        fields,
        repeated_tail,
        span,
    })
}
