//! The typed AST of the input-parser DSL (§7.9).
//!
//! The DSL has its **own** typed AST — the design (§7.9) forbids lowering parser
//! expressions directly into string-splitting calls. The ordinary language
//! parser produces rowan nodes; `praxis-hir` converts those into this `ParserAst`
//! before validation, type synthesis, and plan construction.
//!
//! The M6 subset (§19.6): atomics `int`/`char`/`word`/`text`/`rest`/`digit`;
//! constructors `lines`/`sections`/`csv`/`ws`/`sep`/`grid`; and backtick
//! templates with `{name:parser}` / `{parser}` captures. The advanced
//! constructors (`block`, `choice`, `scan`, heterogeneous `sections`, `optional`,
//! `matrix`, `chars`, `repeated`) land in M9.

use praxis_source::Span;

/// One of the atomic parsers (§7.4). The M6 subset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtomicKind {
    /// Signed decimal integer → `Int`.
    Int,
    /// One Unicode scalar value → `Char`.
    Char,
    /// Non-empty run excluding whitespace and parser-delimiter punctuation → `Text`.
    Word,
    /// Minimally consumes text until the following template literal can match → `Text`.
    Text,
    /// The remainder of the current region → `Text`.
    Rest,
    /// One decimal digit → `Int`.
    Digit,
}

impl AtomicKind {
    /// The source keyword for this atomic.
    pub fn keyword(self) -> &'static str {
        match self {
            AtomicKind::Int => "int",
            AtomicKind::Char => "char",
            AtomicKind::Word => "word",
            AtomicKind::Text => "text",
            AtomicKind::Rest => "rest",
            AtomicKind::Digit => "digit",
        }
    }

    /// Parse an atomic name into its kind, or `None` if unknown.
    pub fn from_keyword(name: &str) -> Option<Self> {
        Some(match name {
            "int" => AtomicKind::Int,
            "char" => AtomicKind::Char,
            "word" => AtomicKind::Word,
            "text" => AtomicKind::Text,
            "rest" => AtomicKind::Rest,
            "digit" => AtomicKind::Digit,
            _ => return None,
        })
    }
}

/// How a template literal run of whitespace matches input (§7.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WsPolicy {
    /// A run of ordinary spaces matches one or more spaces or tabs (the default,
    /// flexible rule for AoC column alignment, §7.2).
    SpaceRun,
    /// `\s*` — zero or more spaces or tabs.
    ZeroOrMore,
    /// `\s+` — one or more spaces or tabs.
    OneOrMore,
    /// `\x20` — exactly one ASCII space.
    ExactSpace,
    /// `\n` — one line ending.
    Newline,
    /// `\t` — one tab.
    Tab,
}

/// One part of a backtick template (§7.9 `TemplatePart`).
#[derive(Clone, Debug)]
pub enum TemplatePart {
    /// A literal run: the raw matched bytes plus the whitespace policy.
    Literal { text: String, ws: WsPolicy },
    /// A capture `{name? : parser}`. `name` is `None` for anonymous captures.
    Capture {
        name: Option<String>,
        parser: Box<ParserAst>,
    },
}

/// A parser expression (§7.9 `ParserExpr`). The M6 subset of the full node list.
#[derive(Clone, Debug)]
pub enum ParserAst {
    /// An atomic parser: `int`, `char`, etc.
    Atomic { kind: AtomicKind, span: Span },
    /// A backtick template: `` `{x:int},{y:int}` ``.
    Template {
        parts: Vec<TemplatePart>,
        span: Span,
    },
    /// `lines(P)` → `Vec[result(P)]`.
    Lines { child: Box<ParserAst>, span: Span },
    /// `sections(P)` → `Vec[result(P)]` (homogeneous; named sections are M9).
    Sections { child: Box<ParserAst>, span: Span },
    /// Named heterogeneous `sections(name: P, ..., tail: repeated(P))` (M9,
    /// §7.5). Each named field parses one fixed section in order; the optional
    /// `repeated_tail` is a named field (`boards: repeated(matrix(int))`) whose
    /// parser consumes all remaining sections as a `Vec[result(P)]`. Result is
    /// an anonymous record `{ field1: result(P1), …, tail_name: Vec[…] }`.
    SectionsNamed {
        /// `(field_name, parser)` pairs in source order. Each parses exactly
        /// one section.
        fields: Vec<(String, ParserAst)>,
        /// The named `repeated(...)` tail, if present. The name (e.g.
        /// `"boards"`) becomes the record field; the parser consumes every
        /// remaining section into a `Vec[result(P)]`.
        repeated_tail: Option<(String, Box<ParserAst>)>,
        span: Span,
    },
    /// `csv(P)` → `Vec[result(P)]`.
    Csv { child: Box<ParserAst>, span: Span },
    /// `ws(P)` → `Vec[result(P)]` (whitespace-separated).
    Ws { child: Box<ParserAst>, span: Span },
    /// `sep(separator, P)` → `Vec[result(P)]`.
    Sep {
        separator: String,
        child: Box<ParserAst>,
        span: Span,
    },
    /// `grid(P)` → `Grid[result(P)]`.
    Grid { child: Box<ParserAst>, span: Span },
    /// `block(item, ...)` (M9, §7.5): apply sequential parsers within one
    /// region. A positional item that is a named-capture template *flattens*
    /// its captures into the block's record; a positional scalar must be
    /// named (else rejected). A named item contributes one field. Result is a
    /// flattened anonymous record.
    Block { items: Vec<BlockItem>, span: Span },
    /// `choice(Name: P, Name: P, ...)` (M9, §7.5): parse one of several
    /// alternatives, generating an anonymous enum. Each case's parser produces
    /// the payload (a record for a named-capture template, a scalar otherwise).
    /// The first alternative that matches wins (source order).
    Choice {
        cases: Vec<(String, ParserAst)>,
        span: Span,
    },
    /// `optional(P)` (M9, §7.5): parse `P` if it matches, else consume nothing
    /// and return `None`. Result is `Option[result(P)]`. Failure consumes no
    /// input (parser-level optionality, not exception recovery).
    Optional { child: Box<ParserAst>, span: Span },
    /// `scan(P)` (M9, §7.5): find repeated `P` matches inside otherwise
    /// irrelevant text (e.g. corrupted AoC input). Returns matches in source
    /// order as `Vec[result(P)]`, ignoring unmatched text.
    Scan { child: Box<ParserAst>, span: Span },
    /// `one_of("LR")` (M9, §7.5): match one character from a literal character
    /// set. Result is `Char`.
    OneOf { chars: String, span: Span },
    /// `chars(P, skip:)` (M9, §7.5): apply a char-parser repeatedly. Result is
    /// `Vec[Char]`. The `skip` policy trims between matches (`none`/`whitespace`/
    /// `newlines`).
    Characters {
        child: Box<ParserAst>,
        skip: SkipPolicy,
        span: Span,
    },
    /// `matrix(P)` (M9, §7.5, ADR-030): parse lines of whitespace-separated
    /// tokens into a rectangular `Grid[result(P)]`. Same result type as `grid`
    /// but tokenizes on whitespace rather than per-character.
    Matrix { child: Box<ParserAst>, span: Span },
    /// Ragged `grid(P, ragged, fill:)` (M9, §7.5): permit uneven rows and pad
    /// to the maximum width with `fill`. The plain `grid(P)` keeps its own arm.
    GridRagged {
        child: Box<ParserAst>,
        /// The fill character/value, as the literal text from source (parsed by
        /// the cell parser at runtime).
        fill: String,
        span: Span,
    },
}

/// How `chars(P, skip:)` trims between matches (§7.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipPolicy {
    /// No trimming between matches.
    None,
    /// Skip horizontal whitespace (spaces/tabs) between matches.
    Whitespace,
    /// Skip any whitespace including newlines between matches.
    Newlines,
}

impl SkipPolicy {
    /// Parse a `skip:` keyword value, or `None` if unknown.
    pub fn from_keyword(name: &str) -> Option<Self> {
        Some(match name {
            "none" => SkipPolicy::None,
            "whitespace" => SkipPolicy::Whitespace,
            "newlines" => SkipPolicy::Newlines,
            _ => return None,
        })
    }
}

/// One item in a `block(...)` (M9, §7.5).
#[derive(Clone, Debug)]
pub enum BlockItem {
    /// A positional parser. If it is a named-capture template, its captures
    /// flatten into the enclosing block record; otherwise (a scalar) it must
    /// be the sole contributor or validation rejects it for an unclear field
    /// name (§7.5).
    Positional(ParserAst),
    /// A named item `name: parser` contributing one field.
    Named { name: String, parser: ParserAst },
}

impl ParserAst {
    /// The source span of this parser node (§7.10: every node carries one).
    pub fn span(&self) -> Span {
        match self {
            ParserAst::Atomic { span, .. }
            | ParserAst::Template { span, .. }
            | ParserAst::Lines { span, .. }
            | ParserAst::Sections { span, .. }
            | ParserAst::SectionsNamed { span, .. }
            | ParserAst::Csv { span, .. }
            | ParserAst::Ws { span, .. }
            | ParserAst::Sep { span, .. }
            | ParserAst::Grid { span, .. }
            | ParserAst::Block { span, .. }
            | ParserAst::Choice { span, .. }
            | ParserAst::Optional { span, .. }
            | ParserAst::Scan { span, .. }
            | ParserAst::OneOf { span, .. }
            | ParserAst::Characters { span, .. }
            | ParserAst::Matrix { span, .. }
            | ParserAst::GridRagged { span, .. } => *span,
        }
    }
}

/// The name of a structural constructor (for dispatch in the parser / validation).
/// Maps an identifier like `"lines"` to its constructor kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Constructor {
    Lines,
    Sections,
    Csv,
    Ws,
    Sep,
    Grid,
}

impl Constructor {
    /// Parse a constructor name, or `None` if unknown / not an M6 constructor.
    pub fn from_keyword(name: &str) -> Option<Self> {
        Some(match name {
            "lines" => Constructor::Lines,
            "sections" => Constructor::Sections,
            "csv" => Constructor::Csv,
            "ws" => Constructor::Ws,
            "sep" => Constructor::Sep,
            "grid" => Constructor::Grid,
            _ => return None,
        })
    }

    /// The expected positional argument count for this constructor.
    pub fn expected_arity(self) -> usize {
        match self {
            Constructor::Lines | Constructor::Sections | Constructor::Csv | Constructor::Ws => 1,
            // sep takes (separator, parser).
            Constructor::Sep => 2,
            Constructor::Grid => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_round_trips_keywords() {
        for kind in [
            AtomicKind::Int,
            AtomicKind::Char,
            AtomicKind::Word,
            AtomicKind::Text,
            AtomicKind::Rest,
            AtomicKind::Digit,
        ] {
            assert_eq!(AtomicKind::from_keyword(kind.keyword()), Some(kind));
        }
        assert_eq!(AtomicKind::from_keyword("nope"), None);
    }

    #[test]
    fn constructor_arity_table() {
        assert_eq!(Constructor::Lines.expected_arity(), 1);
        assert_eq!(Constructor::Sep.expected_arity(), 2);
        assert_eq!(Constructor::Grid.expected_arity(), 1);
    }
}
