//! Semantic tokens (WS8, §15.2, §19.11 criterion 4).
//!
//! Full-document only; deltas are M12.
//!
//! The legend covers §15.2's nine classes. The **four parser classes** —
//! constructors, template literal text, capture names, capture types — read
//! ADR-098's spanned index and nothing else: the compiler is the only thing that
//! knows where a capture's name stops and its type begins, and a second scanner
//! here would be free to disagree with it.
//!
//! The rest walk the rowan tree with `Analysis` beside it, which is what lets
//! the editor tell a `Grid[T]` from a local named `grid`.
//!
//! # Non-overlapping, and single-line
//!
//! The protocol requires tokens to be sorted and disjoint. Parser tokens are
//! collected first and win every overlap, because they are the precise ones —
//! a `BacktickTemplate` is *one* token to the lexer and four to the editor.
//! Anything spanning a line break is dropped: a semantic token cannot cross one,
//! and under ADR-094 no template does.

use lsp_types::{SemanticToken, SemanticTokenType, SemanticTokens, SemanticTokensLegend};
use praxis_hir::SymbolKind;
use praxis_source::Span;
use praxis_syntax::{SyntaxKind, SyntaxToken};
use rowan::NodeOrToken;

use crate::position::Encoding;
use crate::query::Snapshot;

/// The legend, in index order. **The one place the token-type list exists** —
/// the server advertises it, the encoder indexes into it, and WS10's drift gate
/// checks the extension maps every custom entry to a TextMate scope.
pub const TOKEN_TYPES: &[&str] = &[
    // §15.2's classes.
    "keyword",
    "type",
    "function",
    "method",
    "variable",
    "parameter",
    "property",
    "enumMember",
    "number",
    "string",
    // §19.11 criterion 4's four. Custom types rather than standard types with
    // modifiers, because "distinct" is the requirement: a custom type mapped to
    // a scope is distinct in every theme, where a modifier is honoured by few.
    "parserConstructor",
    "parserTemplateText",
    "parserCaptureName",
    "parserCaptureType",
];

/// The four types §19.11 criterion 4 requires, and the ones the extension must
/// map to TextMate scopes (WS10 gate 4).
pub const CUSTOM_TOKEN_TYPES: &[&str] = &[
    "parserConstructor",
    "parserTemplateText",
    "parserCaptureName",
    "parserCaptureType",
];

/// One entry of [`TOKEN_TYPES`], as an index.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TokenType(u32);

impl TokenType {
    /// The index of `name` in the legend, or `None` if the legend has no such
    /// entry — which cannot happen for the constants below and is the reason
    /// they are constants.
    #[must_use]
    pub fn of(name: &str) -> Option<TokenType> {
        TOKEN_TYPES
            .iter()
            .position(|t| *t == name)
            .map(|i| TokenType(u32::try_from(i).unwrap_or(0)))
    }

    #[must_use]
    pub fn index(self) -> u32 {
        self.0
    }

    /// The legend name, for tests that assert a *type* rather than an index.
    #[must_use]
    pub fn name(self) -> &'static str {
        TOKEN_TYPES[self.0 as usize]
    }
}

/// The legend to advertise at `initialize`.
#[must_use]
pub fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: TOKEN_TYPES
            .iter()
            .map(|t| SemanticTokenType::new(t))
            .collect(),
        token_modifiers: Vec::new(),
    }
}

/// One classified range, before relative encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClassifiedToken {
    pub span: Span,
    pub ty: TokenType,
}

/// Every semantic token in the document, in source order and disjoint.
///
/// Exposed as absolute `(span, type)` pairs as well as the encoded form,
/// because the encoded form is deltas and a test that asserts deltas asserts the
/// encoder rather than the classification.
#[must_use]
pub fn classify(snapshot: &Snapshot) -> Vec<ClassifiedToken> {
    let mut parser_tokens = parser_class(snapshot);
    let tree_tokens = tree_class(snapshot);

    parser_tokens.sort_by_key(|t| (t.span.start().to_u32(), t.span.end().to_u32()));

    // Tree tokens lose every overlap with a parser token: a `BacktickTemplate`
    // is one lexer token and several editor tokens, and the constructor name
    // inside a parser expression is an `Ident` the tree would classify twice.
    let mut all: Vec<ClassifiedToken> = parser_tokens.clone();
    all.extend(
        tree_tokens
            .into_iter()
            .filter(|t| !parser_tokens.iter().any(|p| overlaps(p.span, t.span))),
    );
    all.sort_by_key(|t| (t.span.start().to_u32(), t.span.end().to_u32()));

    // Drop anything still overlapping its predecessor and anything empty. Both
    // are protocol violations rather than cosmetic problems: a client that
    // receives overlapping tokens renders unpredictably.
    let mut out: Vec<ClassifiedToken> = Vec::with_capacity(all.len());
    for token in all {
        if token.span.end().to_u32() <= token.span.start().to_u32() {
            continue;
        }
        if let Some(last) = out.last() {
            if last.span.end().to_u32() > token.span.start().to_u32() {
                continue;
            }
        }
        out.push(token);
    }
    out
}

/// The document's semantic tokens, encoded as the protocol's deltas.
#[must_use]
pub fn tokens(snapshot: &Snapshot, enc: Encoding) -> SemanticTokens {
    let positions = snapshot.positions();
    let mut data = Vec::new();
    let (mut last_line, mut last_start) = (0u32, 0u32);

    for token in classify(snapshot) {
        let start = positions.position(token.span.start().to_u32(), enc);
        let end = positions.position(token.span.end().to_u32(), enc);
        // A semantic token cannot cross a line break. Under ADR-094 no template
        // does, and no other classified token spans one either — but dropping
        // rather than truncating keeps a future multi-line construct from
        // producing a token whose length is nonsense.
        if end.line != start.line || end.character <= start.character {
            continue;
        }
        let delta_line = start.line - last_line;
        let delta_start = if delta_line == 0 {
            start.character - last_start
        } else {
            start.character
        };
        data.push(SemanticToken {
            delta_line,
            delta_start,
            length: end.character - start.character,
            token_type: token.ty.index(),
            token_modifiers_bitset: 0,
        });
        last_line = start.line;
        last_start = start.character;
    }

    SemanticTokens {
        result_id: None,
        data,
    }
}

fn overlaps(a: Span, b: Span) -> bool {
    a.start().to_u32() < b.end().to_u32() && b.start().to_u32() < a.end().to_u32()
}

fn ty(name: &str) -> TokenType {
    TokenType::of(name).expect("every name used here is in the legend")
}

/// §19.11 criterion 4's four classes, from ADR-098's index.
fn parser_class(snapshot: &Snapshot) -> Vec<ClassifiedToken> {
    let mut out = Vec::new();
    for index in &snapshot.analyze().parser_exprs {
        for (span, _keyword) in index.constructors() {
            out.push(ClassifiedToken {
                span,
                ty: ty("parserConstructor"),
            });
        }
        for capture in index.captures() {
            if let Some(name) = capture.name_span {
                out.push(ClassifiedToken {
                    span: name,
                    ty: ty("parserCaptureName"),
                });
            }
            out.push(ClassifiedToken {
                span: capture.parser_span,
                ty: ty("parserCaptureType"),
            });
        }
        // …and every atomic, wherever it sits. A capture whose parser is a
        // constructor buries its atomic one level below `captures()`, and
        // `read int` has one in no capture at all; both take the same scope.
        for (span, _kind) in index.atomics() {
            out.push(ClassifiedToken {
                span,
                ty: ty("parserCaptureType"),
            });
        }
        for literal in index.template_literals() {
            out.push(ClassifiedToken {
                span: literal,
                ty: ty("parserTemplateText"),
            });
        }
    }
    // A capture whose parser is itself a constructor (`{xs:csv(int)}`) produces
    // both a capture-type span and a constructor span at the same start. The
    // constructor is the more specific answer, so it wins — sorting by
    // (start, end) puts the narrower one first and the disjointness filter in
    // `classify` drops the wider.
    out.sort_by_key(|t| {
        (
            t.span.start().to_u32(),
            t.span.end().to_u32() - t.span.start().to_u32(),
        )
    });
    out
}

/// The rest of §15.2's classes, from the tree with `Analysis` beside it.
fn tree_class(snapshot: &Snapshot) -> Vec<ClassifiedToken> {
    let analysis = snapshot.analyze();
    let mut out = Vec::new();

    for element in snapshot.tree().descendants_with_tokens() {
        let NodeOrToken::Token(token) = element else {
            continue;
        };
        let Some(name) = classify_token(&token, analysis) else {
            continue;
        };
        let range = token.text_range();
        out.push(ClassifiedToken {
            span: Span::new(u32::from(range.start()), u32::from(range.end())),
            ty: ty(name),
        });
    }
    out
}

fn classify_token(token: &SyntaxToken, analysis: &praxis_hir::Analysis) -> Option<&'static str> {
    let kind = token.kind();
    if kind.is_keyword() {
        return Some("keyword");
    }
    match kind {
        SyntaxKind::IntLit | SyntaxKind::FloatLit => return Some("number"),
        SyntaxKind::TextLit => return Some("string"),
        SyntaxKind::Ident => {}
        _ => return None,
    }

    let range = token.text_range();

    // A method name is not a name reference (HIR-02): it resolves to a catalog
    // entry, so it has its own map and has to be asked first.
    if analysis.method_refs.contains_key(&range) {
        return Some("method");
    }

    let symbol = analysis
        .refs
        .get(&range)
        .map(|r| r.symbol)
        .or_else(|| analysis.decls.get(&range).copied())
        .and_then(|id| analysis.names.get(id));

    if let Some(symbol) = symbol {
        return Some(match symbol.kind {
            SymbolKind::Fn | SymbolKind::Builtin => "function",
            SymbolKind::Struct | SymbolKind::Enum | SymbolKind::BuiltinType => "type",
            SymbolKind::EnumVariant => "enumMember",
            SymbolKind::Param => "parameter",
            SymbolKind::Let | SymbolKind::Var => "variable",
        });
    }

    // An identifier no symbol claims: a record field, in a declaration, a
    // literal, a pattern or an access.
    let parent = token.parent()?;
    match parent.kind() {
        SyntaxKind::FIELD
        | SyntaxKind::PATTERN_FIELD
        | SyntaxKind::FIELD_EXPR
        | SyntaxKind::RECORD_LIT_EXPR => Some("property"),
        // A type name the resolver did not record (an annotation naming a type
        // that does not exist still deserves to look like a type).
        SyntaxKind::TYPE_REF => Some("type"),
        _ => None,
    }
}
