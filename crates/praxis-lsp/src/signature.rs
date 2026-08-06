//! Signature help (WS6, §15.2).
//!
//! Two callee kinds: an ordinary function or method call, whose signature comes
//! from the scheme inference gave it or the catalog entry dispatch selected, and
//! a parser constructor, whose signature is derived from
//! [`Constructor::arg_shape`] — §7.5's own table — so a constructor added later
//! has a signature without anybody writing one.
//!
//! The **active parameter** is computed from the cursor's position among the
//! arguments. That is the part worth testing per position: an implementation
//! that always answers `0` satisfies any "signature help was returned" test.

use lsp_types::{
    Documentation, ParameterInformation, ParameterLabel, SignatureHelp, SignatureInformation,
};
use praxis_input_parser::{ArgShape, Constructor};
use praxis_syntax::{SyntaxKind, SyntaxNode, SyntaxToken};
use rowan::NodeOrToken;

use crate::query::Snapshot;

/// Signature help at `offset`, or `None` when the cursor is not in an argument
/// list.
#[must_use]
pub fn signature_help(snapshot: &Snapshot, offset: u32) -> Option<SignatureHelp> {
    if let Some(list) = snapshot.ancestor_of_kind(offset, SyntaxKind::PARSER_ARG_LIST) {
        return parser_signature(snapshot, &list, offset);
    }
    let list = snapshot.ancestor_of_kind(offset, SyntaxKind::ARG_LIST)?;
    call_signature(snapshot, &list, offset)
}

/// How many top-level commas precede `offset` inside `list`.
///
/// Top-level: a comma nested inside a child node — another call's arguments, a
/// tuple — belongs to that node and is not a separator here, and rowan gives
/// that for free by only walking the list's **direct** children.
fn active_parameter(list: &SyntaxNode, offset: u32) -> u32 {
    let mut n = 0;
    for element in list.children_with_tokens() {
        let NodeOrToken::Token(t) = element else {
            continue;
        };
        if t.kind() != SyntaxKind::COMMA {
            continue;
        }
        if u32::from(t.text_range().end()) <= offset {
            n += 1;
        }
    }
    n
}

fn call_signature(snapshot: &Snapshot, list: &SyntaxNode, offset: u32) -> Option<SignatureHelp> {
    let call = list.parent()?;
    let analysis = snapshot.analyze();
    let db = &analysis.db;

    let (label, params, doc) = match call.kind() {
        SyntaxKind::METHOD_CALL_EXPR => {
            let name = method_name_token(&call)?;
            let m = analysis.method_refs.get(&name.text_range())?;
            let params: Vec<String> = m.entry.params.iter().map(ToString::to_string).collect();
            (
                format!(
                    "{}.{}({}) -> {}",
                    db.render(db.follow(m.receiver)),
                    m.entry.name,
                    params.join(", "),
                    db.render(db.follow(m.result))
                ),
                params,
                Some(m.entry.doc.to_string()),
            )
        }
        SyntaxKind::CALL_EXPR => {
            let callee = call
                .children()
                .find(|c| c.kind() == SyntaxKind::PATH_EXPR)?;
            let name_token = callee
                .children_with_tokens()
                .filter_map(NodeOrToken::into_token)
                .find(|t| t.kind() == SyntaxKind::Ident)?;
            let resolved = analysis.refs.get(&name_token.text_range())?;
            let symbol = analysis.names.get(resolved.symbol)?;
            let scheme = symbol.scheme.as_ref()?;
            let rendered = db.render_scheme(scheme);
            let params = function_params(db, scheme);
            (
                format!("{}: {}", symbol.name, rendered),
                params,
                None::<String>,
            )
        }
        _ => return None,
    };

    Some(one_signature(
        label,
        params,
        doc,
        active_parameter(list, offset),
    ))
}

/// The parameter type strings of a function scheme, or an empty list when the
/// scheme is not a function (a `Vec` constructor, say).
fn function_params(db: &praxis_types::TypeDb, scheme: &praxis_types::Scheme) -> Vec<String> {
    let body = db.follow(scheme.body());
    match db.data(body) {
        praxis_types::data::TypeData::Func { params, .. } => {
            params.iter().map(|p| db.render(db.follow(*p))).collect()
        }
        _ => Vec::new(),
    }
}

fn method_name_token(call: &SyntaxNode) -> Option<SyntaxToken> {
    let mut seen_dot = false;
    for element in call.children_with_tokens() {
        let NodeOrToken::Token(t) = element else {
            continue;
        };
        if t.kind() == SyntaxKind::DOT {
            seen_dot = true;
        } else if seen_dot && t.kind() == SyntaxKind::Ident {
            return Some(t);
        }
    }
    None
}

fn parser_signature(snapshot: &Snapshot, list: &SyntaxNode, offset: u32) -> Option<SignatureHelp> {
    let call = list.parent()?;
    let name = call
        .children()
        .find(|c| c.kind() == SyntaxKind::PATH_EXPR)?
        .text()
        .to_string();
    let ctor = Constructor::from_keyword(name.trim())?;
    let active = active_parameter(list, offset);

    let signatures: Vec<SignatureInformation> = constructor_signatures(ctor)
        .into_iter()
        .map(|(label, params)| signature_info(label, params, None, active))
        .collect();
    let _ = snapshot;
    Some(SignatureHelp {
        signatures,
        active_signature: Some(0),
        active_parameter: Some(active),
    })
}

fn one_signature(
    label: String,
    params: Vec<String>,
    doc: Option<String>,
    active: u32,
) -> SignatureHelp {
    SignatureHelp {
        signatures: vec![signature_info(label, params, doc, active)],
        active_signature: Some(0),
        active_parameter: Some(active),
    }
}

fn signature_info(
    label: String,
    params: Vec<String>,
    doc: Option<String>,
    active: u32,
) -> SignatureInformation {
    SignatureInformation {
        label,
        documentation: doc.map(Documentation::String),
        parameters: Some(
            params
                .into_iter()
                .map(|p| ParameterInformation {
                    label: ParameterLabel::Simple(p),
                    documentation: None,
                })
                .collect(),
        ),
        active_parameter: Some(active),
    }
}

/// The one-line signature shown in completion detail. The first of
/// [`constructor_signatures`], which for `sections` is the homogeneous form.
#[must_use]
pub fn constructor_signature(ctor: Constructor) -> String {
    constructor_signatures(ctor)
        .into_iter()
        .next()
        .map(|(label, _)| label)
        .unwrap_or_else(|| ctor.keyword().to_string())
}

/// Every signature a constructor has, as `(label, parameter labels)`.
///
/// Derived from [`Constructor::arg_shape`] — §7.5's own table — and a result
/// column that is an exhaustive match, so a constructor added later cannot ship
/// without one. `sections` has two, which is §15.2's own example.
#[must_use]
pub fn constructor_signatures(ctor: Constructor) -> Vec<(String, Vec<String>)> {
    let kw = ctor.keyword();
    let result = constructor_result(ctor);
    let one = |args: &[&str], result: &str| {
        let params: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        (format!("{kw}({}) -> {result}", params.join(", ")), params)
    };
    match ctor.arg_shape() {
        ArgShape::Positional(1) => vec![one(&["parser"], result)],
        ArgShape::Positional(n) => {
            // `repeat_n` is 1.82; the workspace MSRV is 1.80 (rust-toolchain.toml).
            let args: Vec<&str> = std::iter::repeat("parser").take(n).collect();
            vec![one(&args, result)]
        }
        ArgShape::StringThenParser => vec![one(&["\"separator\"", "parser"], result)],
        ArgShape::OneString => vec![one(&["\"characters\""], result)],
        ArgShape::ParserWithSkip => vec![one(&["parser", "skip: policy"], result)],
        // Both forms, because which one a reader wants is the whole question:
        // the counted one may be followed by another field and the unbounded
        // one may not.
        ArgShape::ParserWithOptionalCount => {
            vec![one(&["parser"], result), one(&["parser", "count"], result)]
        }
        ArgShape::GridMaybeRagged => vec![
            one(&["parser"], result),
            one(&["parser", "ragged", "fill: value"], result),
        ],
        // §15.2's own pair, in its own words: the homogeneous form first.
        ArgShape::OnePositionalOrNamed => vec![
            one(&["parser"], result),
            (
                format!("{kw}(name: parser, ..., tail: repeated(parser)) -> record"),
                vec![
                    "name: parser".to_string(),
                    "...".to_string(),
                    "tail: repeated(parser)".to_string(),
                ],
            ),
        ],
        ArgShape::Items => vec![one(&["item", "..."], result)],
        ArgShape::NamedOnly { .. } => vec![one(&["Name: parser", "..."], result)],
    }
}

/// The §7.8 result shape of each constructor, as it is written for a reader.
///
/// Exhaustive: a constructor added to §7.5 has to say what it produces here, in
/// the same edit that gives it a `synthesize` arm.
fn constructor_result(ctor: Constructor) -> &'static str {
    match ctor {
        Constructor::Lines
        | Constructor::Sections
        | Constructor::Csv
        | Constructor::Ws
        | Constructor::Sep
        | Constructor::Scan
        | Constructor::Chars
        | Constructor::Repeated => "Vec[T]",
        Constructor::Grid | Constructor::Matrix => "Grid[T]",
        Constructor::OneOf => "Char",
        Constructor::Block => "record",
        Constructor::Choice => "enum",
        Constructor::Optional => "Option[T]",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §15.2's three examples, verbatim in shape.
    #[test]
    fn the_design_documents_three_examples_are_the_signatures() {
        let sections = constructor_signatures(Constructor::Sections);
        assert_eq!(sections[0].0, "sections(parser) -> Vec[T]");
        assert_eq!(
            sections[1].0,
            "sections(name: parser, ..., tail: repeated(parser)) -> record"
        );
        assert_eq!(
            constructor_signatures(Constructor::Lines)[0].0,
            "lines(parser) -> Vec[T]"
        );
    }

    /// `repeated` has two forms and the editor offers both, because the choice
    /// between them is the one a reader is actually making: a counted group is
    /// bounded, so a field may follow it, and an uncounted one is not.
    #[test]
    fn repeated_offers_the_counted_form_beside_the_greedy_one() {
        let repeated = constructor_signatures(Constructor::Repeated);
        assert_eq!(repeated.len(), 2);
        assert_eq!(repeated[0].0, "repeated(parser) -> Vec[T]");
        assert_eq!(repeated[1].0, "repeated(parser, count) -> Vec[T]");
    }

    /// Every §7.5 constructor has a signature, because the derivation is a sweep
    /// over the closed table and not a list somebody maintains.
    #[test]
    fn every_constructor_has_at_least_one_signature() {
        for ctor in Constructor::ALL {
            let sigs = constructor_signatures(*ctor);
            assert!(!sigs.is_empty(), "`{}` has no signature", ctor.keyword());
            assert!(
                sigs[0].0.starts_with(ctor.keyword()),
                "`{}`'s signature must name it",
                ctor.keyword()
            );
        }
    }
}
