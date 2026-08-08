//! Static validation of a parser AST (§7.9 step 2).
//!
//! Catches structural errors before type synthesis / plan construction: mixed
//! named/anonymous captures in one template (§7.3), duplicate capture names, and
//! wrong constructor arity.
//!
//! Returns lightweight [`ValidationError`]s carrying the source [`Span`] (byte
//! offsets). The HIR layer, which knows the [`FileId`], converts these into full
//! [`Diagnostic`]s with the `I0xx` (input-parser) category.

use crate::ast::{ArgShape, Constructor, ParserAst, TemplatePart};
use praxis_source::{DiagCode, Span};

/// A structural error found while validating a parser AST.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationError {
    /// The byte span of the offending node.
    pub span: Span,
    /// Which diagnostic this is.
    pub code: DiagCode,
    /// A human-readable explanation.
    pub message: String,
}

/// Validate a parser AST. Returns the list of errors (empty on success).
pub fn validate(ast: &ParserAst) -> Vec<ValidationError> {
    let mut errs = Vec::new();
    validate_node(ast, &mut errs);
    errs
}

fn validate_node(ast: &ParserAst, errs: &mut Vec<ValidationError>) {
    match ast {
        ParserAst::Atomic { .. } => {}
        ParserAst::Template { parts, span } => {
            validate_template(parts, *span, errs);
        }
        // Every constructor whose only structure is its one child. The extra
        // payload some of them carry is not this pass's to check, and is
        // already answered where the node is *built*: `sep`'s separator by
        // `Separator::new`, the type's only constructor, which refuses the `""`
        // that never advances a cursor; `chars`'s skip policy and ragged
        // `grid`'s fill by `build_call`. A check `validate` performs is one the
        // next construction site can forget.
        ParserAst::Lines { child, .. }
        | ParserAst::Sections { child, .. }
        | ParserAst::Csv { child, .. }
        | ParserAst::Ws { child, .. }
        | ParserAst::Grid { child, .. }
        | ParserAst::Sep { child, .. }
        | ParserAst::Optional { child, .. }
        | ParserAst::Scan { child, .. }
        | ParserAst::Characters { child, .. }
        | ParserAst::Matrix { child, .. }
        | ParserAst::GridRagged { child, .. } => {
            validate_node(child, errs);
        }
        ParserAst::SectionsNamed {
            fields,
            repeated_tail,
            span,
        } => {
            // At least one named field is required (§7.5: a named sections call
            // with zero fields is malformed). The test is `fields`, not "no
            // named argument at all": `sections(boards: repeated(P))` is a
            // greedy tail and nothing else, which is spelled `sections(P)` and
            // is still refused here. A *counted* group is an ordinary field —
            // it is in `fields` — so `sections(shapes: repeated(P, 6))` alone
            // is legal, which is the point of it being bounded.
            if fields.is_empty() {
                errs.push(ValidationError {
                    span: *span,
                    code: DiagCode::EmptyFieldList,
                    message: "named `sections` requires at least one field".to_string(),
                });
            }
            // Field names must be unique.
            let mut seen = Vec::new();
            for item in fields {
                let name = item.name();
                if seen.iter().any(|s: &String| s == name) {
                    errs.push(ValidationError {
                        span: *span,
                        code: DiagCode::DuplicateSectionField,
                        message: format!("duplicate section field `{name}`"),
                    });
                }
                seen.push(name.to_string());
                validate_node(item.parser(), errs);
            }
            // The tail is a field of the generated record too, so its name
            // shares the uniqueness check: `sections(items: lines(int), items:
            // repeated(int))` would otherwise synthesize a record with two
            // fields called `items`.
            if let Some((name, tail)) = repeated_tail {
                if seen.contains(name) {
                    errs.push(ValidationError {
                        span: *span,
                        code: DiagCode::DuplicateSectionField,
                        message: format!("duplicate section field `{name}`"),
                    });
                }
                seen.push(name.clone());
                validate_node(tail, errs);
            }
        }
        ParserAst::Block { items, span } => {
            // §7.5: a positional parser returning a scalar must be explicitly
            // named to avoid an unclear field name. A positional template with
            // named captures flattens its captures into the block record. All
            // flattened field names must be unique.
            let mut seen: Vec<String> = Vec::new();
            for item in items {
                match item {
                    crate::ast::BlockItem::Positional(p) => {
                        // Collect the field names this positional contributes.
                        let contributed = block_positional_field_names(p);
                        // §7.5: a positional parser returning a *scalar* must be
                        // explicitly named (unclear field name). A template —
                        // even one with no captures (a pure literal match) — is
                        // fine: a no-capture template contributes no fields but
                        // legitimately consumes input; a named-capture template
                        // flattens its fields.
                        let is_template = matches!(p, ParserAst::Template { .. });
                        if contributed.is_empty() && !is_template {
                            errs.push(ValidationError {
                                span: *span,
                                code: DiagCode::UnnamedScalarBlockItem,
                                message:
                                    "a positional `block` item returning a scalar must be named"
                                        .to_string(),
                            });
                        }
                        for n in &contributed {
                            if seen.contains(n) {
                                errs.push(ValidationError {
                                    span: *span,
                                    code: DiagCode::DuplicateSectionField,
                                    message: format!("duplicate block field `{n}`"),
                                });
                            }
                            seen.push(n.clone());
                        }
                        validate_node(p, errs);
                    }
                    crate::ast::BlockItem::Named { name, parser } => {
                        if seen.contains(name) {
                            errs.push(ValidationError {
                                span: *span,
                                code: DiagCode::DuplicateSectionField,
                                message: format!("duplicate block field `{name}`"),
                            });
                        }
                        seen.push(name.clone());
                        validate_node(parser, errs);
                    }
                }
            }
        }
        ParserAst::Choice { cases, span } => {
            // §7.5: at least one case; unique case names; recurse.
            if cases.is_empty() {
                errs.push(ValidationError {
                    span: *span,
                    code: DiagCode::EmptyFieldList,
                    message: "`choice` requires at least one case".to_string(),
                });
            }
            let mut seen = Vec::new();
            for (name, parser) in cases {
                if seen.contains(name) {
                    errs.push(ValidationError {
                        span: *span,
                        code: DiagCode::DuplicateChoiceCase,
                        message: format!("duplicate choice case `{name}`"),
                    });
                }
                seen.push(name.clone());
                validate_node(parser, errs);
            }
        }
        ParserAst::OneOf { .. } => {}
    }
}

/// The field names a positional `block` item contributes via flattening (§7.5).
/// A named-capture template contributes its capture names; anything else (a
/// scalar atomic, a constructor) contributes nothing — which means a bare
/// scalar positional must be explicitly named (validation rejects it).
fn block_positional_field_names(p: &ParserAst) -> Vec<String> {
    match p {
        ParserAst::Template { parts, .. } => parts
            .iter()
            .filter_map(|part| match part {
                TemplatePart::Capture { name: Some(n), .. } => Some(n.as_str().to_string()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Validate a template: no mixing named and anonymous captures (§7.3), and no
/// duplicate capture names.
fn validate_template(parts: &[TemplatePart], span: Span, errs: &mut Vec<ValidationError>) {
    let mut has_named = false;
    let mut has_anonymous = false;
    for part in parts {
        if let TemplatePart::Capture { name, parser, .. } = part {
            if name.is_some() {
                has_named = true;
            } else {
                has_anonymous = true;
            }
            validate_node(parser, errs);
        }
    }
    if has_named && has_anonymous {
        errs.push(ValidationError {
            span,
            code: DiagCode::MixedCaptureNaming,
            message: "named and anonymous captures may not be mixed in one template (§7.3)"
                .to_string(),
        });
    }

    // Named captures must have unique names within a template.
    let mut seen_names = Vec::new();
    for part in parts {
        if let TemplatePart::Capture { name: Some(n), .. } = part {
            if seen_names.contains(n) {
                errs.push(ValidationError {
                    span,
                    code: DiagCode::DuplicateCaptureName,
                    message: format!("duplicate capture name `{n}` in template"),
                });
            }
            seen_names.push(n.clone());
        }
    }
}

/// What one argument of a constructor call *is*, with no payload — enough to
/// decide whether the call has the shape §7.5 gives it.
///
/// The payloads live in `praxis-hir`'s `CallArg` (they hold a `ParserAst` and
/// the rowan text). This is the projection both callers can share: the HIR
/// bridge and the capture-body parser in [`crate::body`] check the same table,
/// so the two grammars cannot drift.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArgKind {
    /// A positional parser expression.
    Parser,
    /// A positional string literal.
    String,
    /// A positional whole-number literal — the count of `repeated(P, N)`.
    Int,
    /// A bare keyword flag, e.g. the `ragged` of `grid(P, ragged, fill: 0)`.
    Flag(String),
    /// A named argument `name: parser` — the value is a parser expression.
    Named(String),
    /// A named argument `name: keyword` whose value is a **keyword, not a
    /// parser**: `chars`'s `skip:`, `grid`'s `fill:`.
    ///
    /// Distinct from [`ArgKind::Named`] because [`check_call`] must tell the
    /// two apart: only the constructors §7.5 gives a keyword argument accept
    /// one, and `block`, `choice` and named `sections` must refuse it.
    Keyword(String),
    /// A named `name: repeated(P)` tail.
    RepeatedTail(String),
}

impl ArgKind {
    pub(crate) fn describe(&self) -> String {
        match self {
            ArgKind::Parser => "a parser".to_string(),
            ArgKind::String => "a string literal".to_string(),
            ArgKind::Int => "a whole-number literal".to_string(),
            ArgKind::Flag(f) => format!("the flag `{f}`"),
            ArgKind::Named(n) => format!("the named argument `{n}:`"),
            ArgKind::Keyword(n) => format!("the keyword argument `{n}:`"),
            ArgKind::RepeatedTail(n) => format!("the repeated tail `{n}:`"),
        }
    }
}

/// Check a constructor call against §7.5's shape for that constructor —
/// **before anything is built**.
///
/// Returns every problem found; an empty vector means the argument list has
/// exactly the shape the constructor's builder expects, so the builder has
/// nothing left to drop. Each constructor gets its own [`ArgShape`] arm, and
/// each argument is checked *in place* rather than only counted: `choice(int)`
/// (a positional in a named-only constructor) and `sep(int, int)` (a parser
/// where a separator belongs) both have the right arity and the wrong
/// arguments.
pub fn check_call(ctor: Constructor, args: &[ArgKind], span: Span) -> Vec<ValidationError> {
    let mut errs = Vec::new();
    let name = ctor.keyword();
    let arity = |errs: &mut Vec<ValidationError>, expected: &str, actual: usize| {
        errs.push(ValidationError {
            span,
            code: DiagCode::ConstructorArity,
            message: format!("`{name}` expects {expected}, got {actual}"),
        });
    };
    let bad_arg = |errs: &mut Vec<ValidationError>, at: usize, arg: &ArgKind, wanted: &str| {
        errs.push(ValidationError {
            span,
            code: DiagCode::InvalidConstructorArgument,
            message: format!(
                "`{name}` argument {} is {}, but {wanted}",
                at + 1,
                arg.describe()
            ),
        });
    };

    match ctor.arg_shape() {
        ArgShape::Positional(n) => {
            if args.len() != n {
                arity(
                    &mut errs,
                    &format!("{n} argument{}", if n == 1 { "" } else { "s" }),
                    args.len(),
                );
            }
            for (i, a) in args.iter().enumerate() {
                if *a != ArgKind::Parser {
                    bad_arg(&mut errs, i, a, "every argument must be a parser");
                }
            }
        }
        ArgShape::StringThenParser => {
            if args.len() != 2 {
                arity(&mut errs, "2 arguments", args.len());
            }
            for (i, a) in args.iter().enumerate() {
                let wanted = if i == 0 {
                    ("the separator must be a string literal", ArgKind::String)
                } else {
                    ("the element parser must be a parser", ArgKind::Parser)
                };
                if *a != wanted.1 {
                    bad_arg(&mut errs, i, a, wanted.0);
                }
            }
        }
        ArgShape::OneString => {
            if args.len() != 1 {
                arity(&mut errs, "1 argument", args.len());
            }
            for (i, a) in args.iter().enumerate() {
                if *a != ArgKind::String {
                    bad_arg(
                        &mut errs,
                        i,
                        a,
                        "the character set must be a string literal",
                    );
                }
            }
        }
        ArgShape::ParserWithSkip => {
            match args.first() {
                Some(ArgKind::Parser) => {}
                Some(other) => bad_arg(&mut errs, 0, other, "the first argument must be a parser"),
                None => arity(&mut errs, "1 or 2 arguments", 0),
            }
            for (i, a) in args.iter().enumerate().skip(1) {
                match a {
                    // The keyword is the constructor's (`keyword_arg`), not a
                    // name this table repeats: one table entry, one spelling.
                    ArgKind::Keyword(n) if Some(n.as_str()) == ctor.keyword_arg() && i == 1 => {}
                    other => bad_arg(&mut errs, i, other, "only `skip:` may follow the parser"),
                }
            }
            if args.len() > 2 {
                arity(&mut errs, "1 or 2 arguments", args.len());
            }
        }
        ArgShape::ParserWithOptionalCount => {
            // The arity check comes **first**, unlike the arms above, because
            // one caller reads only the first error: the capture-body scanner
            // turns a shape failure into a single `ScanError::CallShape`. A
            // wrong number of arguments is the more useful of the two things a
            // three-argument `repeated` is wrong about, and it is what the
            // rowan front end reports for it.
            if args.is_empty() || args.len() > 2 {
                arity(&mut errs, "1 or 2 arguments", args.len());
            }
            match args.first() {
                // An empty list is the arity error above and nothing else.
                None | Some(ArgKind::Parser) => {}
                Some(other) => {
                    bad_arg(&mut errs, 0, other, "the repeated parser must be a parser");
                }
            }
            match args.get(1) {
                None | Some(ArgKind::Int) => {}
                // The reason is in the message because there is no other
                // spelling that would have worked: the parser plan is built
                // when the program is compiled, so a count read from a value
                // cannot exist.
                Some(other) => bad_arg(
                    &mut errs,
                    1,
                    other,
                    "the count must be a whole-number literal — the parser plan is built when \
                     the program is compiled, so the count cannot be a parser or a variable",
                ),
            }
        }
        ArgShape::GridMaybeRagged => {
            match args.first() {
                Some(ArgKind::Parser) => {}
                Some(other) => {
                    bad_arg(&mut errs, 0, other, "the cell parser must be a parser");
                }
                None => arity(&mut errs, "1 or 3 arguments", 0),
            }
            let mut ragged = false;
            let mut fill = false;
            for (i, a) in args.iter().enumerate().skip(1) {
                match a {
                    // Both names come from the constructor — `flag_arg` for
                    // `ragged`, `keyword_arg` for `fill:` — so this table and
                    // the two front ends that mint the arguments read one
                    // spelling between them.
                    ArgKind::Flag(f) if Some(f.as_str()) == ctor.flag_arg() && !ragged => {
                        ragged = true
                    }
                    ArgKind::Keyword(n) if Some(n.as_str()) == ctor.keyword_arg() && !fill => {
                        fill = true
                    }
                    other => bad_arg(
                        &mut errs,
                        i,
                        other,
                        "only `ragged` and `fill:` may follow the cell parser",
                    ),
                }
            }
            // §7.5 spells them together: `grid(P, ragged, fill: value)`.
            // Accepting a `fill:` with no `ragged` would silently build the
            // ragged form, a different parser than the one written.
            if ragged != fill {
                errs.push(ValidationError {
                    span,
                    code: DiagCode::InvalidConstructorArgument,
                    message:
                        "`grid`'s ragged form is written `grid(P, ragged, fill: value)` — `ragged` \
                         and `fill:` come together or not at all (§7.5)"
                            .to_string(),
                });
            }
        }
        ArgShape::OnePositionalOrNamed => {
            let named = args
                .iter()
                .filter(|a| matches!(a, ArgKind::Named(_) | ArgKind::RepeatedTail(_)))
                .count();
            if named == 0 {
                // Homogeneous `sections(P)`.
                if args.len() != 1 {
                    arity(&mut errs, "1 argument, or named sections", args.len());
                }
                for (i, a) in args.iter().enumerate() {
                    if *a != ArgKind::Parser {
                        bad_arg(&mut errs, i, a, "the section parser must be a parser");
                    }
                }
            } else {
                // Heterogeneous `sections(name: P, …)`: named arguments only,
                // and every one of them names a *parser*. `sections` has no
                // keyword argument (`Constructor::keyword_arg`), so a keyword
                // reaching here is one no constructor asked for.
                for (i, a) in args.iter().enumerate() {
                    if !matches!(a, ArgKind::Named(_) | ArgKind::RepeatedTail(_)) {
                        bad_arg(
                            &mut errs,
                            i,
                            a,
                            "a named `sections` takes only named arguments",
                        );
                    }
                }
            }
        }
        ArgShape::Items => {
            if args.is_empty() {
                arity(&mut errs, "at least 1 item", 0);
            }
            for (i, a) in args.iter().enumerate() {
                if !matches!(a, ArgKind::Parser | ArgKind::Named(_)) {
                    bad_arg(
                        &mut errs,
                        i,
                        a,
                        "a `block` item is a parser or a named parser",
                    );
                }
            }
        }
        ArgShape::NamedOnly { at_least } => {
            let named = args
                .iter()
                .filter(|a| matches!(a, ArgKind::Named(_)))
                .count();
            if named < at_least {
                arity(
                    &mut errs,
                    &format!("at least {at_least} named argument(s)"),
                    named,
                );
            }
            for (i, a) in args.iter().enumerate() {
                if !matches!(a, ArgKind::Named(_)) {
                    bad_arg(&mut errs, i, a, "every argument must be `Name: parser`");
                }
            }
        }
    }
    errs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{
        AtomicKind, CaptureName, EmptySeparator, RepeatCount, SectionItem, Separator,
    };
    use praxis_source::{DiagCode, Span};

    fn atom() -> ParserAst {
        ParserAst::Atomic {
            kind: AtomicKind::Int,
            span: Span::at(0),
        }
    }

    #[test]
    fn clean_tree_validates() {
        let ast = ParserAst::Lines {
            child: Box::new(atom()),
            span: Span::at(0),
        };
        assert!(validate(&ast).is_empty());
    }

    #[test]
    fn mixed_captures_rejected() {
        let ast = ParserAst::Template {
            parts: vec![
                TemplatePart::Capture {
                    name: Some(CaptureName::parse("x").expect("an identifier")),
                    parser: Box::new(atom()),
                    span: Span::at(0),
                    name_span: None,
                },
                TemplatePart::Capture {
                    name: None,
                    parser: Box::new(atom()),
                    span: Span::at(0),
                    name_span: None,
                },
            ],
            span: Span::at(0),
        };
        let errs = validate(&ast);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].code, DiagCode::MixedCaptureNaming);
    }

    #[test]
    fn duplicate_named_capture_rejected() {
        let ast = ParserAst::Template {
            parts: vec![
                TemplatePart::Capture {
                    name: Some(CaptureName::parse("x").expect("an identifier")),
                    parser: Box::new(atom()),
                    span: Span::at(0),
                    name_span: None,
                },
                TemplatePart::Capture {
                    name: Some(CaptureName::parse("x").expect("an identifier")),
                    parser: Box::new(atom()),
                    span: Span::at(0),
                    name_span: None,
                },
            ],
            span: Span::at(0),
        };
        let errs = validate(&ast);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].code, DiagCode::DuplicateCaptureName);
    }

    #[test]
    fn arity_mismatch_reported() {
        let errs = check_call(Constructor::Sep, &[ArgKind::String], Span::at(0));
        assert!(!errs.is_empty());
        assert_eq!(errs[0].code, DiagCode::ConstructorArity);
    }

    /// **A keyword argument is not a named parser.**
    ///
    /// `CallArg::Keyword{name}` and `CallArg::Named{name}` project onto
    /// distinct `ArgKind`s so [`check_call`] can tell them apart: only the
    /// constructors §7.5 gives a keyword argument accept one, and `block`,
    /// `choice` and named `sections` refuse it rather than having their
    /// builders `filter_map` it away.
    ///
    /// The last two assertions are the ones that make this a test of the
    /// *distinction* rather than of a blanket refusal: the same position,
    /// holding a named parser, is still accepted.
    #[test]
    fn a_keyword_argument_is_accepted_only_where_the_shape_has_one() {
        let kw = |n: &str| ArgKind::Keyword(n.to_string());
        let named = |n: &str| ArgKind::Named(n.to_string());

        // The two constructors §7.5 gives a keyword argument.
        assert!(check_call(
            Constructor::Chars,
            &[ArgKind::Parser, kw("skip")],
            Span::at(0)
        )
        .is_empty());
        assert!(check_call(
            Constructor::Grid,
            &[
                ArgKind::Parser,
                ArgKind::Flag("ragged".to_string()),
                kw("fill")
            ],
            Span::at(0)
        )
        .is_empty());

        // Everywhere else — including the *other* constructor's keyword.
        for (ctor, args) in [
            (Constructor::Block, vec![ArgKind::Parser, kw("fill")]),
            (Constructor::Choice, vec![named("A"), kw("fill")]),
            (Constructor::Sections, vec![named("rules"), kw("fill")]),
            (Constructor::Lines, vec![kw("skip")]),
            (Constructor::Chars, vec![ArgKind::Parser, kw("fill")]),
        ] {
            let errs = check_call(ctor, &args, Span::at(0));
            assert!(
                errs.iter()
                    .any(|e| e.code == DiagCode::InvalidConstructorArgument),
                "`{}` must refuse a keyword it does not have",
                ctor.keyword()
            );
        }

        // The same shape with a named *parser* there is fine.
        assert!(check_call(
            Constructor::Block,
            &[ArgKind::Parser, named("fill")],
            Span::at(0)
        )
        .is_empty());
        assert!(check_call(
            Constructor::Sections,
            &[named("rules"), named("fill")],
            Span::at(0)
        )
        .is_empty());
    }

    /// **A keyword argument's *value* is part of its shape.**
    ///
    /// [`check_call`] answers from `ArgKind`s, which carry names and no values,
    /// so the shape table can never see this — the check belongs to the
    /// builder. `chars`'s `skip:` refuses a policy it does not recognize;
    /// `grid`'s `fill:` refuses an empty pad, the same rule one field over from
    /// `Separator::new` refusing `""`: an empty separator never advances, and
    /// an empty pad fills nothing.
    ///
    /// The builder is shared, so both front ends inherit whatever it decides —
    /// which is the point of asserting it here rather than at either one.
    #[test]
    fn a_keyword_argument_with_no_value_is_not_a_shape() {
        use crate::call::{build_call, CallArg};

        let grid = |fill: &str| {
            build_call(
                Constructor::Grid,
                vec![
                    CallArg::Parser(ParserAst::Atomic {
                        kind: crate::ast::AtomicKind::Char,
                        span: Span::at(0),
                    }),
                    CallArg::Flag("ragged".to_string()),
                    CallArg::Keyword {
                        name: "fill".to_string(),
                        value: fill.to_string(),
                    },
                ],
                Span::at(0),
            )
        };

        for empty in ["", "\"\""] {
            let errs = grid(empty).expect_err("an empty fill pads nothing");
            assert_eq!(errs[0].code, DiagCode::InvalidConstructorArgument);
        }

        // And the values that *are* values still build, decoded.
        for (written, decoded) in [("0", "0"), ("\"-\"", "-"), ("\" \"", " ")] {
            match grid(written).expect("a fill with a value") {
                ParserAst::GridRagged { fill, .. } => assert_eq!(fill, decoded),
                other => panic!("`fill: {written}` built a {other:?}"),
            }
        }
    }

    /// **The assertion is inverted on purpose.**
    ///
    /// An empty separator drives `walk_sep`'s
    /// `region[pos..].starts_with(sep_bytes)` loop, which is unconditionally
    /// true for an empty needle, so the cursor never advances and the loop
    /// allocates forever. A check `validate` performs is one the *next*
    /// construction site can forget, so the rule the name states is enforced by
    /// the type instead: `Separator` has exactly one constructor and it refuses
    /// `""`. The empty separator is not rejected before plan construction — it
    /// is not constructible at all.
    #[test]
    fn empty_separator_is_rejected_before_plan_construction() {
        assert_eq!(
            Separator::new(""),
            Err(EmptySeparator),
            "the one constructor refuses the separator that cannot advance"
        );

        let comma = Separator::new(",").expect("a one-character separator is fine");
        assert_eq!(comma.as_str(), ",");

        // The AST field is the newtype, not a `String`: this is what stops the
        // empty value from being reintroduced by a future construction site.
        let ast = ParserAst::Sep {
            separator: comma,
            child: Box::new(atom()),
            span: Span::at(0),
        };
        assert!(validate(&ast).is_empty(), "a real separator validates");
    }

    #[test]
    fn repeated_section_tail_cannot_reuse_a_fixed_field_name() {
        let ast = ParserAst::SectionsNamed {
            fields: vec![SectionItem::One {
                name: "items".to_string(),
                parser: atom(),
            }],
            repeated_tail: Some(("items".to_string(), Box::new(atom()))),
            span: Span::at(0),
        };

        let errors = validate(&ast);
        assert!(
            errors
                .iter()
                .any(|error| error.code == DiagCode::DuplicateSectionField),
            "the generated record cannot contain two fields named `items`"
        );
    }

    /// A counted group is a record field like any other, so it collides with
    /// the tail's name the same way a fixed field does. The duplicate check
    /// reads `SectionItem::name`, which is one answer for both variants — a
    /// check that only knew about `One` would let `Counted` past.
    #[test]
    fn a_counted_group_and_the_tail_cannot_share_a_name() {
        let ast = ParserAst::SectionsNamed {
            fields: vec![SectionItem::Counted {
                name: "shapes".to_string(),
                count: RepeatCount::new(2).expect("two sections"),
                parser: atom(),
            }],
            repeated_tail: Some(("shapes".to_string(), Box::new(atom()))),
            span: Span::at(0),
        };

        let errors = validate(&ast);
        assert!(
            errors
                .iter()
                .any(|error| error.code == DiagCode::DuplicateSectionField),
            "a counted group's name is a field name too"
        );
    }

    /// **A counted group alone is a `sections` call; an unbounded tail alone is
    /// not.** `sections(boards: repeated(P))` is `sections(P)` written the long
    /// way round — every section, one parser — which is what I025 says. A
    /// counted group consumes a *known* prefix and leaves the rest unread, so
    /// it is a heterogeneous call with one field, and the same check must let
    /// it through. That is why the emptiness test is `fields`, which holds the
    /// counted item and not the tail.
    #[test]
    fn a_counted_group_alone_is_a_field_but_an_unbounded_tail_alone_is_not() {
        let counted_only = ParserAst::SectionsNamed {
            fields: vec![SectionItem::Counted {
                name: "shapes".to_string(),
                count: RepeatCount::new(2).expect("two sections"),
                parser: atom(),
            }],
            repeated_tail: None,
            span: Span::at(0),
        };
        assert!(validate(&counted_only).is_empty());

        let tail_only = ParserAst::SectionsNamed {
            fields: Vec::new(),
            repeated_tail: Some(("boards".to_string(), Box::new(atom()))),
            span: Span::at(0),
        };
        assert!(
            validate(&tail_only)
                .iter()
                .any(|e| e.code == DiagCode::EmptyFieldList),
            "a greedy tail and nothing else is `sections(P)`"
        );
    }

    /// The shape table owns `repeated`'s arity, not the marker's builder —
    /// ADR-073's "check the shape before building" covers this call site like
    /// every other, and this is the table answering.
    #[test]
    fn repeated_takes_a_parser_and_an_optional_count() {
        let ok = |args: &[ArgKind]| check_call(Constructor::Repeated, args, Span::at(0));
        assert!(ok(&[ArgKind::Parser]).is_empty());
        assert!(ok(&[ArgKind::Parser, ArgKind::Int]).is_empty());

        // No parser at all, and a count where the parser belongs.
        assert!(ok(&[]).iter().any(|e| e.code == DiagCode::ConstructorArity));
        assert!(ok(&[ArgKind::Int])
            .iter()
            .any(|e| e.code == DiagCode::InvalidConstructorArgument));
        // A second parser is not a count — this is the diagnostic a
        // non-literal `repeated(P, n)` earns.
        assert!(ok(&[ArgKind::Parser, ArgKind::Parser])
            .iter()
            .any(|e| e.code == DiagCode::InvalidConstructorArgument));
        assert!(ok(&[ArgKind::Parser, ArgKind::Int, ArgKind::Int])
            .iter()
            .any(|e| e.code == DiagCode::ConstructorArity));
    }
}
