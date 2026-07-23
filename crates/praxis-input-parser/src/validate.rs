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
use praxis_source::Span;

/// A structural error found while validating a parser AST.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationError {
    /// The byte span of the offending node.
    pub span: Span,
    /// A machine-readable code (e.g. `"I020"`).
    pub code: &'static str,
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
            code: "I020",
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
                    code: "I021",
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
            code: "I022",
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
    use crate::ast::AtomicKind;
    use praxis_source::Span;

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
        assert_eq!(errs[0].code, "I020");
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
        assert_eq!(errs[0].code, "I021");
    }

    #[test]
    fn arity_mismatch_reported() {
        let err = check_constructor_arity(Constructor::Sep, 1, Span::at(0));
        assert!(err.is_some());
        assert_eq!(err.unwrap().code, "I022");
    }
}
