//! The retained parser AST and its per-node types (ADR-098, §15.3).
//!
//! Inference used to convert a `read`/`parse` body into a
//! [`ParserAst`](praxis_input_parser::ParserAst), read one type off the root and
//! **drop the tree**. Four M11 deliverables need what was dropped: hover on an
//! inner constructor, completion inside `{…}` and after `read`, the four parser
//! semantic-token classes (§19.11 criterion 4), and §15.3's five-way "which mode
//! is the cursor in" question.
//!
//! This module is the answer to all four, and it is deliberately *here* rather
//! than in `praxis-lsp`: the compiler already knows where every capture, name
//! and capture type ends, and a second scanner in the language server would be
//! free to disagree with it. See ADR-098.

use praxis_input_parser::{AtomicKind, BlockItem, ParserAst, SectionItem, TemplatePart};
use praxis_source::Span;
use praxis_types::Type;
use rowan::TextRange;

/// One `read`/`parse` body's retained analysis.
///
/// Every span in here is **absolute** — a file offset, not relative to a
/// template interior — because `ParserAst` spans already are (§7.10, ADR-078)
/// and the template scanner's are rebased by `shift_part_spans` before the tree
/// leaves `convert_template`. So a span in this index is directly usable as an
/// LSP range with no arithmetic at the boundary.
#[derive(Clone, Debug)]
pub struct ParserIndex {
    /// The `PARSER_EXPR` node's own range: the whole of what follows `read`, or
    /// `parse`'s second argument.
    pub expr_range: TextRange,
    /// The converted tree.
    pub ast: ParserAst,
    /// The type synthesized for each AST node, keyed by that node's span, in
    /// **post-order** — children before parents. Where two nodes share a span
    /// (`` `{int}` `` is a template whose extent equals its only capture's), the
    /// earlier entry is the deeper node.
    pub node_types: Vec<(Span, Type)>,
}

/// Where the cursor is inside a `read`/`parse` body (§15.3's five-way question).
///
/// The order of the variants is the order §15.3 lists them, and the answer is
/// the **innermost** one that applies: a cursor inside `{n:int}`'s `int` is
/// `AtomicName`, not `Capture`, even though both contain it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ParserMode {
    /// Not inside any `read`/`parse` parser expression.
    Outside,
    /// In parser-expression mode: inside a `read`/`parse` body but not inside a
    /// backtick template.
    Expression,
    /// Inside a backtick template, but not inside one of its captures.
    Template,
    /// Inside a capture's braces, but not on its parser.
    Capture,
    /// Inside a capture's parser name — the atomic (`int`) or constructor
    /// (`csv`) the capture body names.
    AtomicName,
}

impl ParserIndex {
    /// Whether `offset` falls inside this parser expression.
    #[must_use]
    pub fn contains(&self, offset: u32) -> bool {
        let (s, e) = (
            u32::from(self.expr_range.start()),
            u32::from(self.expr_range.end()),
        );
        (s..=e).contains(&offset)
    }

    /// The synthesized type of the **innermost** parser node containing
    /// `offset`, or `None` when no node does.
    ///
    /// "Innermost" is the smallest containing span, and ties go to the earlier
    /// entry — which is the deeper node, because the recording is post-order.
    /// Without the tie-break, hovering `` read `{int}` `` would answer the
    /// template's type or the capture's depending on vector order, and the two
    /// are the same only by coincidence.
    #[must_use]
    pub fn type_at(&self, offset: u32) -> Option<Type> {
        let mut best: Option<(u32, Type)> = None;
        for (span, ty) in &self.node_types {
            if !span_contains(*span, offset) {
                continue;
            }
            let width = span.end().to_u32().saturating_sub(span.start().to_u32());
            match best {
                // Strictly smaller wins; equal width does **not**, so the first
                // (deepest) entry of a tie is kept.
                Some((w, _)) if width >= w => {}
                _ => best = Some((width, *ty)),
            }
        }
        best.map(|(_, t)| t)
    }

    /// The type of the parser node whose span is exactly `span`, if the index
    /// has one. Used where the caller already has a node's extent (a semantic
    /// token, say) and wants that node's type and no other's.
    #[must_use]
    pub fn type_of_span(&self, span: Span) -> Option<Type> {
        self.node_types
            .iter()
            .find(|(s, _)| *s == span)
            .map(|(_, t)| *t)
    }

    /// §15.3's five-way question, answered against the spans the compiler
    /// computed.
    #[must_use]
    pub fn mode_at(&self, offset: u32) -> ParserMode {
        if !self.contains(offset) {
            return ParserMode::Outside;
        }
        let mut mode = ParserMode::Expression;
        walk_mode(&self.ast, offset, &mut mode);
        mode
    }

    /// The capture containing `offset`, if any: its name span (when named) and
    /// its parser's span.
    #[must_use]
    pub fn capture_at(&self, offset: u32) -> Option<CaptureAt> {
        let mut found = None;
        walk_captures(&self.ast, &mut |cap: CaptureAt| {
            if span_contains(cap.span, offset) {
                // Innermost wins: a nested template's capture is visited after
                // the capture that encloses it.
                found = Some(cap);
            }
        });
        found
    }

    /// Every capture in this parser expression, outermost first, each with the
    /// spans the editor needs to colour it (§19.11 criterion 4).
    #[must_use]
    pub fn captures(&self) -> Vec<CaptureAt> {
        let mut out = Vec::new();
        walk_captures(&self.ast, &mut |cap| out.push(cap));
        out
    }

    /// Every literal run in every template in this parser expression, with the
    /// source extent it was decoded from. Empty-extent runs (a policy escape
    /// contributes text of its own) are included; the caller decides.
    #[must_use]
    pub fn template_literals(&self) -> Vec<Span> {
        let mut out = Vec::new();
        walk_literals(&self.ast, &mut |span| out.push(span));
        out
    }

    /// Every constructor node's name span and keyword, outermost first.
    ///
    /// The name is the first thing in a constructor call — `lines(` — so its
    /// extent is the node's start plus the keyword's length. That is arithmetic
    /// on a closed table (`Constructor::keyword`), not a scan.
    #[must_use]
    pub fn constructors(&self) -> Vec<(Span, &'static str)> {
        let mut out = Vec::new();
        walk_constructors(&self.ast, &mut |span, kw| out.push((span, kw)));
        out
    }

    /// Every §7.4 atomic node's span and keyword, anywhere in the expression.
    ///
    /// Not the same set as the captures' parser spans: a capture whose parser is
    /// a *constructor* — `{xs:csv(int)}` — has an atomic buried one level down
    /// that `captures()` does not reach, and a top-level `read int` has one that
    /// is in no capture at all. Both are the `support.type.capture.praxis` scope
    /// the grammar paints, so the editor colours them the same either way.
    #[must_use]
    pub fn atomics(&self) -> Vec<(Span, AtomicKind)> {
        let mut out = Vec::new();
        walk_atomics(&self.ast, &mut |span, kind| out.push((span, kind)));
        out
    }
}

/// One capture's spans.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureAt {
    /// The whole capture, both braces included.
    pub span: Span,
    /// The capture name, trimmed. `None` for an anonymous capture.
    pub name_span: Option<Span>,
    /// The capture's parser — the `int` of `{n:int}`.
    pub parser_span: Span,
}

/// Whether `span` covers `offset`. The end is **inclusive**, because a cursor
/// sitting just past the last byte of a name is still "in" it as far as an
/// editor is concerned — that is where the caret is after typing it.
fn span_contains(span: Span, offset: u32) -> bool {
    (span.start().to_u32()..=span.end().to_u32()).contains(&offset)
}

/// Narrow `mode` as the walk descends into a template, a capture, and a
/// capture's parser name.
fn walk_mode(ast: &ParserAst, offset: u32, mode: &mut ParserMode) {
    if let ParserAst::Template { parts, span } = ast {
        if span_contains(*span, offset) {
            *mode = ParserMode::Template;
            for part in parts {
                let TemplatePart::Capture { parser, span, .. } = part else {
                    continue;
                };
                if !span_contains(*span, offset) {
                    continue;
                }
                *mode = ParserMode::Capture;
                if span_contains(parser.span(), offset) {
                    *mode = ParserMode::AtomicName;
                }
                // A capture body may hold a template of its own (D10), and the
                // cursor may be inside that one instead.
                walk_mode(parser, offset, mode);
            }
        }
        return;
    }
    for child in children(ast) {
        walk_mode(child, offset, mode);
    }
}

fn walk_captures(ast: &ParserAst, f: &mut impl FnMut(CaptureAt)) {
    if let ParserAst::Template { parts, .. } = ast {
        for part in parts {
            if let TemplatePart::Capture {
                parser,
                span,
                name_span,
                ..
            } = part
            {
                f(CaptureAt {
                    span: *span,
                    name_span: *name_span,
                    parser_span: parser.span(),
                });
                walk_captures(parser, f);
            }
        }
        return;
    }
    for child in children(ast) {
        walk_captures(child, f);
    }
}

fn walk_literals(ast: &ParserAst, f: &mut impl FnMut(Span)) {
    if let ParserAst::Template { parts, .. } = ast {
        for part in parts {
            match part {
                TemplatePart::Literal { span, .. } => f(*span),
                TemplatePart::Capture { parser, .. } => walk_literals(parser, f),
            }
        }
        return;
    }
    for child in children(ast) {
        walk_literals(child, f);
    }
}

fn walk_atomics(ast: &ParserAst, f: &mut impl FnMut(Span, AtomicKind)) {
    if let ParserAst::Atomic { kind, span } = ast {
        f(*span, *kind);
        return;
    }
    if let ParserAst::Template { parts, .. } = ast {
        for part in parts {
            if let TemplatePart::Capture { parser, .. } = part {
                walk_atomics(parser, f);
            }
        }
        return;
    }
    for child in children(ast) {
        walk_atomics(child, f);
    }
}

fn walk_constructors(ast: &ParserAst, f: &mut impl FnMut(Span, &'static str)) {
    if let Some(kw) = constructor_keyword(ast) {
        let start = ast.span().start().to_u32();
        f(
            Span::new(start, start + u32::try_from(kw.len()).unwrap_or(0)),
            kw,
        );
    }
    if let ParserAst::Template { parts, .. } = ast {
        for part in parts {
            if let TemplatePart::Capture { parser, .. } = part {
                walk_constructors(parser, f);
            }
        }
        return;
    }
    for child in children(ast) {
        walk_constructors(child, f);
    }
}

/// The §7.5 constructor keyword this node was written with, or `None` for the
/// two nodes that are not constructor calls (an atomic and a template).
///
/// **Read from `Constructor`, never from a name list** — the same closed table
/// `Constructor::from_keyword` builds these nodes out of, so a constructor added
/// later cannot be highlighted in the editor and unknown here.
fn constructor_keyword(ast: &ParserAst) -> Option<&'static str> {
    use praxis_input_parser::Constructor as C;
    Some(
        match ast {
            ParserAst::Atomic { .. } | ParserAst::Template { .. } => return None,
            ParserAst::Lines { .. } => C::Lines,
            ParserAst::Sections { .. } | ParserAst::SectionsNamed { .. } => C::Sections,
            ParserAst::Csv { .. } => C::Csv,
            ParserAst::Ws { .. } => C::Ws,
            ParserAst::Sep { .. } => C::Sep,
            ParserAst::Grid { .. } | ParserAst::GridRagged { .. } => C::Grid,
            ParserAst::Block { .. } => C::Block,
            ParserAst::Choice { .. } => C::Choice,
            ParserAst::Optional { .. } => C::Optional,
            ParserAst::Scan { .. } => C::Scan,
            ParserAst::OneOf { .. } => C::OneOf,
            ParserAst::Characters { .. } => C::Chars,
            ParserAst::Matrix { .. } => C::Matrix,
        }
        .keyword(),
    )
}

/// This node's parser children, in source order. `Template` is handled by each
/// walker directly, because its children hang off `TemplatePart`s.
fn children(ast: &ParserAst) -> Vec<&ParserAst> {
    match ast {
        ParserAst::Atomic { .. } | ParserAst::OneOf { .. } | ParserAst::Template { .. } => {
            Vec::new()
        }
        ParserAst::Lines { child, .. }
        | ParserAst::Sections { child, .. }
        | ParserAst::Csv { child, .. }
        | ParserAst::Ws { child, .. }
        | ParserAst::Sep { child, .. }
        | ParserAst::Grid { child, .. }
        | ParserAst::Optional { child, .. }
        | ParserAst::Scan { child, .. }
        | ParserAst::Characters { child, .. }
        | ParserAst::Matrix { child, .. }
        | ParserAst::GridRagged { child, .. } => vec![child],
        ParserAst::SectionsNamed {
            fields,
            repeated_tail,
            ..
        } => fields
            .iter()
            .map(SectionItem::parser)
            .chain(repeated_tail.iter().map(|(_, t)| &**t))
            .collect(),
        ParserAst::Block { items, .. } => items
            .iter()
            .map(|i| match i {
                BlockItem::Positional(p) | BlockItem::Named { parser: p, .. } => p,
            })
            .collect(),
        ParserAst::Choice { cases, .. } => cases.iter().map(|(_, p)| p).collect(),
    }
}
