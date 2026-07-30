//! Static validation of a parser AST (§7.9 step 2).
//!
//! Catches structural errors before type synthesis / plan construction: mixed
//! named/anonymous captures in one template (§7.3), duplicate capture names, and
//! wrong constructor arity.
//!
//! Returns lightweight [`ValidationError`]s carrying the source [`Span`] (byte
//! offsets). The HIR layer, which knows the [`FileId`], converts these into full
//! [`Diagnostic`]s with the `I0xx` (input-parser) category.

use crate::ast::{Constructor, ParserAst, TemplatePart};
use praxis_source::{DiagCode, Span};

/// A structural error found while validating a parser AST.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationError {
    /// The byte span of the offending node.
    pub span: Span,
    /// Which diagnostic this is. A registered [`DiagCode`] rather than the
    /// `&'static str` it used to be: the string was parsed back into a number
    /// by `praxis-hir` to build the real code, so a typo in it was a silent
    /// `I000` (F2).
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
        ParserAst::Lines { child, .. }
        | ParserAst::Sections { child, .. }
        | ParserAst::Csv { child, .. }
        | ParserAst::Ws { child, .. }
        | ParserAst::Grid { child, .. } => {
            validate_node(child, errs);
        }
        ParserAst::Sep { child, .. } => {
            validate_node(child, errs);
        }
        ParserAst::SectionsNamed {
            fields,
            repeated_tail,
            span,
        } => {
            // At least one named field is required (§7.5: a named sections call
            // with zero fields is malformed).
            if fields.is_empty() {
                errs.push(ValidationError {
                    span: *span,
                    code: DiagCode::EmptyFieldList,
                    message: "named `sections` requires at least one field".to_string(),
                });
            }
            // Field names must be unique.
            let mut seen = Vec::new();
            for (name, child) in fields {
                if seen.contains(name) {
                    errs.push(ValidationError {
                        span: *span,
                        code: DiagCode::DuplicateSectionField,
                        message: format!("duplicate section field `{name}`"),
                    });
                }
                seen.push(name.clone());
                validate_node(child, errs);
            }
            // The tail is a field of the generated record too (IP-09). It used
            // to be validated for its *parser* and never for its *name*, so
            // `sections(items: lines(int), items: repeated(int))` synthesized a
            // record with two fields called `items`.
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
        ParserAst::Optional { child, .. } => validate_node(child, errs),
        ParserAst::Scan { child, .. } => validate_node(child, errs),
        ParserAst::Characters { child, .. } => validate_node(child, errs),
        ParserAst::Matrix { child, .. } => validate_node(child, errs),
        ParserAst::GridRagged { child, .. } => validate_node(child, errs),
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
                TemplatePart::Capture { name: Some(n), .. } => Some(n.clone()),
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
        if let TemplatePart::Capture { name, parser } = part {
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

/// Validate a constructor call's arity. Returns the error (if any) so the caller
/// can report it with the call-site span.
pub fn check_constructor_arity(
    ctor: Constructor,
    actual: usize,
    span: Span,
) -> Option<ValidationError> {
    let expected = ctor.expected_arity();
    if actual != expected {
        Some(ValidationError {
            span,
            code: DiagCode::ConstructorArity,
            message: format!(
                "`{}` expects {} argument{}, got {}",
                ctor_name(ctor),
                expected,
                if expected == 1 { "" } else { "s" },
                actual
            ),
        })
    } else {
        None
    }
}

fn ctor_name(c: Constructor) -> &'static str {
    match c {
        Constructor::Lines => "lines",
        Constructor::Sections => "sections",
        Constructor::Csv => "csv",
        Constructor::Ws => "ws",
        Constructor::Sep => "sep",
        Constructor::Grid => "grid",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{AtomicKind, EmptySeparator, Separator};
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
                    name: Some("x".to_string()),
                    parser: Box::new(atom()),
                },
                TemplatePart::Capture {
                    name: None,
                    parser: Box::new(atom()),
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
                    name: Some("x".to_string()),
                    parser: Box::new(atom()),
                },
                TemplatePart::Capture {
                    name: Some("x".to_string()),
                    parser: Box::new(atom()),
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
        let err = check_constructor_arity(Constructor::Sep, 1, Span::at(0));
        assert!(err.is_some());
        assert_eq!(err.unwrap().code, DiagCode::ConstructorArity);
    }

    /// **The assertion is inverted on purpose (IP-10).**
    ///
    /// This test used to build `ParserAst::Sep { separator: String::new(), … }`
    /// and ask whether `validate` reported it. That question presumes the value
    /// exists — and the value is the hazard: an empty separator drives
    /// `walk_sep`'s `region[pos..].starts_with(sep_bytes)` loop, which is
    /// unconditionally true for an empty needle, so the cursor never advances
    /// and the loop allocates forever. A check `validate` performs is one the
    /// *next* construction site can forget.
    ///
    /// So the rule the name states is now enforced by the type: `Separator` has
    /// exactly one constructor and it refuses `""`. The empty separator is not
    /// rejected before plan construction — it is not constructible at all, and
    /// the `String::new()` this test used to write no longer compiles.
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
            fields: vec![("items".to_string(), atom())],
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
}
