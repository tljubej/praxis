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

/// One of the atomic parsers (§7.4).
///
/// **§7.4's list is closed, and this is all ten of it.** Four were missing
/// (IP-11) — `uint`, `float`, `byte`, `identifier` — so a program that wrote
/// one got "unknown atomic parser" for a name the design document requires.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AtomicKind {
    /// Signed decimal integer → `Int`.
    Int,
    /// Non-negative decimal integer → `Int`.
    ///
    /// **`Int`, not `ScalarType::UInt`.** `UInt` is reserved and has no runtime
    /// object at all: `praxis_repr::builtin_for_type` answers `NoRuntimeRepr`
    /// for it, and under D9 a JIT compile fails when a descriptor is missing —
    /// so a `uint` capture typed `UInt` would make every program containing one
    /// fail to compile. The non-negativity is enforced by the *parse rule*
    /// (a leading `-` is refused), which is what §7.4 asks for.
    UInt,
    /// Decimal floating-point number → `Float`.
    Float,
    /// A decimal integer in `0..=255` → `Byte`.
    ///
    /// A decimal integer and not a raw input byte: a raw byte cannot be
    /// re-sliced as `Text` without breaking the UTF-8 invariant every
    /// source-slice `Text` relies on.
    Byte,
    /// One Unicode scalar value → `Char`.
    Char,
    /// One decimal digit → `Int`.
    Digit,
    /// Non-empty run excluding whitespace and parser-delimiter punctuation → `Text`.
    Word,
    /// An identifier run → `Text`.
    ///
    /// §7.4 says "ASCII-like identifier syntax by default"; this uses §4.1's
    /// **one** identifier class (`praxis_syntax::ident`), which is a deliberate
    /// widening. A parser that accepted a narrower set of names than the
    /// language itself does would refuse identifiers a Praxis program can
    /// declare, and F3 exists precisely so there is not a second rule.
    Identifier,
    /// Minimally consumes text until the following template literal can match → `Text`.
    Text,
    /// The remainder of the current region → `Text`.
    Rest,
}

impl AtomicKind {
    /// The source keyword for this atomic.
    ///
    /// The **only** place these ten strings are spelled. Completion labels,
    /// hover, and the runtime's `expected …` parse-fault names all read them
    /// from here; `praxis_runtime::parser::walk_atomic` used to re-spell them
    /// per-arm, and the copy shared by `text` and `rest` had already drifted to
    /// naming `text` for both.
    pub fn keyword(self) -> &'static str {
        match self {
            AtomicKind::Int => "int",
            AtomicKind::UInt => "uint",
            AtomicKind::Float => "float",
            AtomicKind::Byte => "byte",
            AtomicKind::Char => "char",
            AtomicKind::Digit => "digit",
            AtomicKind::Word => "word",
            AtomicKind::Identifier => "identifier",
            AtomicKind::Text => "text",
            AtomicKind::Rest => "rest",
        }
    }

    /// One line of §7.4, for hover. Exhaustive, for the reason
    /// [`Constructor::doc`] is.
    pub fn doc(self) -> &'static str {
        match self {
            AtomicKind::Int => {
                "Signed decimal integer. Surrounding horizontal space is the \
                 caller's, not the atomic's."
            }
            AtomicKind::UInt => "Non-negative decimal integer; a leading `-` is refused.",
            AtomicKind::Float => "Decimal floating-point number.",
            AtomicKind::Byte => "A decimal integer in `0..=255` — a number, not a raw input byte.",
            AtomicKind::Char => "One Unicode scalar value, whitespace included where offered.",
            AtomicKind::Digit => "One decimal digit.",
            AtomicKind::Word => {
                "A non-empty run excluding whitespace and parser-delimiter punctuation."
            }
            AtomicKind::Identifier => "An identifier, by §4.1's own identifier rule.",
            AtomicKind::Text => {
                "Consumes as little as possible until the literal run that \
                 follows can match."
            }
            AtomicKind::Rest => "The remainder of the current region.",
        }
    }

    /// Parse an atomic name into its kind, or `None` if unknown.
    pub fn from_keyword(name: &str) -> Option<Self> {
        Some(match name {
            "int" => AtomicKind::Int,
            "uint" => AtomicKind::UInt,
            "float" => AtomicKind::Float,
            "byte" => AtomicKind::Byte,
            "char" => AtomicKind::Char,
            "digit" => AtomicKind::Digit,
            "word" => AtomicKind::Word,
            "identifier" => AtomicKind::Identifier,
            "text" => AtomicKind::Text,
            "rest" => AtomicKind::Rest,
            _ => return None,
        })
    }

    /// Every atomic, in §7.4's order. The list is **closed**: a test sweeps it,
    /// so a new atomic cannot be added without a type and a runtime rule.
    pub const ALL: &'static [AtomicKind] = &[
        AtomicKind::Int,
        AtomicKind::UInt,
        AtomicKind::Float,
        AtomicKind::Byte,
        AtomicKind::Char,
        AtomicKind::Digit,
        AtomicKind::Word,
        AtomicKind::Identifier,
        AtomicKind::Text,
        AtomicKind::Rest,
    ];
}

/// How a template literal run of whitespace matches input (§7.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WsPolicy {
    /// The literal had no whitespace run in front of it, so no whitespace is
    /// consumed before matching it.
    ///
    /// Added by S20 (IPR-12). Before it, every literal was tagged
    /// [`SpaceRun`](Self::SpaceRun) whether or not the template had written a
    /// space run, so the interpreter could not tell "this literal had a leading
    /// run" from "it did not" — and the only way to keep templates matching was
    /// to implement `SpaceRun` as zero-or-more, contradicting its own
    /// definition. This variant is what lets `SpaceRun` mean what it says.
    None,
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

/// The name of a template capture — **an identifier by construction** (IP-04).
///
/// §4.1 allows Unicode identifiers and F3 gave the workspace one character
/// class for them. The scanner used to carry a private ASCII copy of the rule,
/// so `{λ:int}` was not recognized as a *named* capture at all: the whole body
/// `λ:int` was silently reinterpreted as the parser expression. A name a
/// consumer cannot accept must be reported, never rewritten into a different
/// name — see `praxis_syntax::ident::is_ident`, which is the predicate.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CaptureName(Box<str>);

/// The one way [`CaptureName::parse`] fails: the text is not an identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidCaptureName;

impl CaptureName {
    /// The **only** constructor.
    ///
    /// # Errors
    /// [`InvalidCaptureName`] when `text` is not a §4.1 identifier.
    pub fn parse(text: &str) -> Result<Self, InvalidCaptureName> {
        if praxis_syntax::ident::is_ident(text) {
            Ok(CaptureName(text.into()))
        } else {
            Err(InvalidCaptureName)
        }
    }

    /// The name text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CaptureName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One part of a backtick template (§7.9 `TemplatePart`).
///
/// **Every part carries its own source span**, for the same reason every
/// [`ParserAst`] node does (§7.10, ADR-078): the editor has to colour the
/// capture *name* differently from the capture *type* (§19.11 criterion 4), and
/// the only alternative to recording the extent where the scanner already knows
/// it is a second scanner in the language server re-deriving it — the failure
/// ADR-098 exists to prevent. Spans are interior-relative until
/// [`shift_part_spans`] rebases them onto the file, exactly like the spans on
/// the parser nodes underneath.
#[derive(Clone, Debug)]
pub enum TemplatePart {
    /// A literal run: the raw matched bytes plus the whitespace policy.
    ///
    /// `span` covers the **source** the run was decoded from, which is not the
    /// same length as `text`: `` \` `` is two source bytes and one character,
    /// and a policy part decoded from `\s+` has an empty `text` and a two-byte
    /// span.
    Literal {
        text: String,
        ws: WsPolicy,
        span: Span,
    },
    /// A capture `{name? : parser}`. `name` is `None` for anonymous captures.
    ///
    /// `parser` is the capture's **own** parser expression, parsed from its own
    /// body (IP-05, D10). It used to be a placeholder `Atomic { Int }` that the
    /// HIR overwrote by rescanning the whole template and taking the first
    /// recognizable name — so every capture in a template shared one kind.
    ///
    /// `span` covers the whole capture including both braces; `name_span`
    /// covers the name **as trimmed** (`{ n :int}` names `n`, not `" n "`), and
    /// is `None` exactly when `name` is. The capture's *type* needs no span
    /// here: it is `parser.span()`.
    Capture {
        name: Option<CaptureName>,
        parser: Box<ParserAst>,
        span: Span,
        name_span: Option<Span>,
    },
}

impl TemplatePart {
    /// The part's source span.
    #[must_use]
    pub fn span(&self) -> Span {
        match self {
            TemplatePart::Literal { span, .. } | TemplatePart::Capture { span, .. } => *span,
        }
    }
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
    /// §7.5). Result is an anonymous record with one field per named argument,
    /// in source order.
    ///
    /// A named argument takes one of three forms, and the split between
    /// `fields` and `repeated_tail` is what keeps the third one's rule
    /// structural rather than remembered:
    ///
    /// - `name: P` — one section, one field of `result(P)`
    ///   ([`SectionItem::One`]);
    /// - `name: repeated(P, N)` — exactly `N` consecutive sections, one field
    ///   of `Vec[result(P)]` ([`SectionItem::Counted`]). It is **bounded**, so
    ///   it may sit anywhere among the named arguments and other fields may
    ///   follow it;
    /// - `name: repeated(P)` — every section that is left, one field of
    ///   `Vec[result(P)]`. It is greedy, so nothing can follow it: there is at
    ///   most one and it is last, which is `repeated_tail` being a single
    ///   `Option` outside the list rather than a variant inside it.
    SectionsNamed {
        /// The named arguments other than the unbounded tail, in source order.
        /// Each contributes exactly one record field and consumes
        /// [`SectionItem::sections_wanted`] sections.
        fields: Vec<SectionItem>,
        /// The unbounded `repeated(P)` tail, if present. The name (e.g.
        /// `"boards"`) becomes the record's last field; the parser consumes
        /// every remaining section into a `Vec[result(P)]`.
        repeated_tail: Option<(String, Box<ParserAst>)>,
        span: Span,
    },
    /// `csv(P)` → `Vec[result(P)]`.
    Csv { child: Box<ParserAst>, span: Span },
    /// `ws(P)` → `Vec[result(P)]` (whitespace-separated).
    Ws { child: Box<ParserAst>, span: Span },
    /// `sep(separator, P)` → `Vec[result(P)]`. The separator is a
    /// [`Separator`], which cannot be empty (IP-10).
    Sep {
        separator: Separator,
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
    /// `Vec[result(P)]` (D-S20-A) — **not** `Vec[Char]` whatever `P` is, which
    /// is what `synthesize` said while the runtime stored what `P` produced, so
    /// `chars(int, skip: none)` advertised a `Vec[Char]` full of `Int` objects.
    /// `chars(one_of("LR"))` is still `Vec[Char]`, because `one_of` is `Char`.
    /// The `skip` policy trims between matches; see [`SkipPolicy`], and note
    /// that `newlines` is the *broader* of the two non-`none` policies.
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

/// The separator of a `sep(separator, P)` call — **non-empty by construction**
/// (IP-10).
///
/// An empty separator is not a parser that matches nothing: it is a cursor that
/// never advances. `walk_sep` in `praxis-runtime` asks
/// `region[pos..].starts_with(sep_bytes)`, which is *unconditionally true* for
/// an empty needle, so `pos += sep_bytes.len()` is `pos += 0` and the loop
/// pushes a freshly allocated value forever — an infinite loop that also grows
/// the heap without bound.
///
/// A `validate` check would have caught the value only where someone remembered
/// to call it; the type catches it at every construction site there will ever
/// be, which is the house maxim (`AGENTS.md`: make illegal states
/// unrepresentable).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Separator(Box<str>);

/// The one way [`Separator::new`] fails: the text was empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EmptySeparator;

impl std::fmt::Display for EmptySeparator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a `sep` separator may not be empty: it could never advance")
    }
}

impl std::error::Error for EmptySeparator {}

impl Separator {
    /// The **only** constructor. Refuses the empty string.
    ///
    /// # Errors
    /// [`EmptySeparator`] when `text` is empty.
    pub fn new(text: &str) -> Result<Self, EmptySeparator> {
        if text.is_empty() {
            return Err(EmptySeparator);
        }
        Ok(Separator(text.into()))
    }

    /// The separator text. Never empty.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Separator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The `N` of a `repeated(P, N)` — **at least one section, by construction**.
///
/// A group of no sections parses nothing: `repeated(P, 0)` would produce an
/// empty `Vec` while consuming no input, which is not a parser anybody means to
/// write and reads as a typo for the unbounded form. A negative count names no
/// sections at all. Both are the same kind of value [`Separator`] refuses one
/// field over — a number the runtime would have to invent a meaning for — and
/// they are refused the same way, by the one constructor, rather than by a
/// `validate` arm the next construction site can forget.
///
/// The upper bound is the plan's: [`crate::plan::SectionItemNode`] stores the
/// count as a `u32`, because the plan is a flat `&'static` repr the runtime
/// reads without allocating. A count that does not fit is refused here, where
/// the source span is still in hand, rather than truncated there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepeatCount(std::num::NonZeroU32);

/// The two ways [`RepeatCount::new`] fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvalidRepeatCount {
    /// Zero or negative: a group of no sections parses nothing.
    NotPositive,
    /// Larger than a `u32`, which is what the plan node holds.
    TooLarge,
}

impl std::fmt::Display for InvalidRepeatCount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            InvalidRepeatCount::NotPositive => {
                "a `repeated` count must be at least 1: a group of no sections parses nothing"
            }
            InvalidRepeatCount::TooLarge => "a `repeated` count must fit in 32 bits",
        })
    }
}

impl std::error::Error for InvalidRepeatCount {}

impl RepeatCount {
    /// The **only** constructor. Refuses a count that names no sections.
    ///
    /// # Errors
    /// [`InvalidRepeatCount`] for `n <= 0` or `n > u32::MAX`.
    pub fn new(n: i64) -> Result<Self, InvalidRepeatCount> {
        if n <= 0 {
            return Err(InvalidRepeatCount::NotPositive);
        }
        let n = u32::try_from(n).map_err(|_| InvalidRepeatCount::TooLarge)?;
        // Non-zero by the check above; `NonZeroU32::new` cannot fail here.
        std::num::NonZeroU32::new(n)
            .map(RepeatCount)
            .ok_or(InvalidRepeatCount::NotPositive)
    }

    /// The count. Never zero.
    #[must_use]
    pub fn get(self) -> u32 {
        self.0.get()
    }
}

impl std::fmt::Display for RepeatCount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.get())
    }
}

/// One named argument of a heterogeneous `sections(...)` other than its
/// unbounded tail (§7.5).
///
/// Each variant contributes exactly one field to the generated record; they
/// differ in how many sections they consume, which is what
/// [`sections_wanted`](Self::sections_wanted) answers. The count lives *in the
/// item* rather than in a parallel position map beside the field list, because
/// a position recorded twice is a position that can disagree with itself —
/// which is the drift ADR-073 was written about.
#[derive(Clone, Debug)]
pub enum SectionItem {
    /// `name: P` — one section, one field of `result(P)`.
    One { name: String, parser: ParserAst },
    /// `name: repeated(P, N)` — exactly `N` consecutive sections, one field of
    /// `Vec[result(P)]`.
    Counted {
        name: String,
        count: RepeatCount,
        parser: ParserAst,
    },
}

impl SectionItem {
    /// The record field this item contributes.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            SectionItem::One { name, .. } | SectionItem::Counted { name, .. } => name,
        }
    }

    /// The parser applied to each of this item's sections.
    #[must_use]
    pub fn parser(&self) -> &ParserAst {
        match self {
            SectionItem::One { parser, .. } | SectionItem::Counted { parser, .. } => parser,
        }
    }

    /// The parser, mutably — for [`ParserAst::shift_spans`].
    pub fn parser_mut(&mut self) -> &mut ParserAst {
        match self {
            SectionItem::One { parser, .. } | SectionItem::Counted { parser, .. } => parser,
        }
    }

    /// How many sections this item consumes.
    #[must_use]
    pub fn sections_wanted(&self) -> usize {
        match self {
            SectionItem::One { .. } => 1,
            SectionItem::Counted { count, .. } => count.get() as usize,
        }
    }
}

/// How `chars(P, skip:)` trims between matches (§7.5).
///
/// **Read the two non-`None` variants as an inclusion, because the names do not
/// say so.** `Whitespace` is *horizontal* whitespace; `Newlines` is horizontal
/// whitespace **and** line endings. So `Newlines` skips strictly more than
/// `Whitespace` — `whitespace` is the narrower policy despite being the broader
/// English word, and `skip: newlines` means "newlines *as well*", not "newlines
/// only".
///
/// That inversion is not academic: it is what made
/// `chars(one_of("^v<>"), skip: whitespace)` — §7.5's own example — look like it
/// should absorb an input file's trailing `\n`, and a stage shipped believing
/// it. It does not, and it does not have to: the terminator is **inside** the
/// root region — the root region is the whole buffer, and nothing is trimmed off
/// it — and it is forgiven because it is whitespace the character parser
/// declined (`walk_characters` asks the child first and accepts a
/// whitespace-only leftover through `ByteRegion::is_all_whitespace`, the bound
/// half of ADR-078's rule). No skip policy has to account for it. (An earlier
/// version of this note said the terminator was *outside* the root region, which
/// was round two's answer — a trim of exactly one terminator, deleted because a
/// file ending `"\n\n"` defeated it.) The sets are kept as they are because they are the ones
/// §7.5's example needs and swapping them would silently change what every
/// existing `skip: newlines` program accepts; what was missing was this
/// paragraph. `walk_characters`/`skip_chars` in `praxis-runtime` is the
/// implementation, and `SkipPolicy::skips` below is the single description both
/// the runtime comment and the `skip:` diagnostic quote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipPolicy {
    /// No trimming between matches: every byte of the region is the child's.
    None,
    /// Skip **horizontal** whitespace — spaces and tabs — between matches. Not
    /// line endings: see the type's own documentation.
    Whitespace,
    /// Skip horizontal whitespace **and** line endings between matches. The
    /// broader of the two policies.
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

    /// What this policy skips, in the words the `skip:` diagnostic uses.
    ///
    /// One description, quoted by the diagnostic and by the runtime, so a
    /// reader who reaches either one is told that `newlines` is the broader
    /// policy rather than left to infer it from the names.
    pub fn skips(self) -> &'static str {
        match self {
            SkipPolicy::None => "nothing",
            SkipPolicy::Whitespace => "spaces and tabs",
            SkipPolicy::Newlines => "spaces, tabs and line endings",
        }
    }

    /// Every policy, in §7.5's order. The list is **closed**: a test sweeps it.
    pub const ALL: &'static [SkipPolicy] = &[
        SkipPolicy::None,
        SkipPolicy::Whitespace,
        SkipPolicy::Newlines,
    ];
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

    /// Shift every span in this subtree by `delta` bytes.
    ///
    /// The template scanner works in **interior-relative** offsets: it is given
    /// the text between the backticks and knows nothing about where the token
    /// sits in the file. The HIR bridge, which does know, rebases the tree by
    /// the token's start + 1 (the opening backtick). Without this a capture
    /// body's diagnostic caret would land near the top of the file.
    pub fn shift_spans(&mut self, delta: u32) {
        // Bind the span mutably in one place, then recurse into the children.
        match self {
            ParserAst::Atomic { span, .. } | ParserAst::OneOf { span, .. } => {
                *span = span.shifted(delta);
            }
            ParserAst::Template { parts, span } => {
                *span = span.shifted(delta);
                shift_part_spans(parts, delta);
            }
            ParserAst::Lines { child, span }
            | ParserAst::Sections { child, span }
            | ParserAst::Csv { child, span }
            | ParserAst::Ws { child, span }
            | ParserAst::Grid { child, span }
            | ParserAst::Sep { child, span, .. }
            | ParserAst::Optional { child, span }
            | ParserAst::Scan { child, span }
            | ParserAst::Matrix { child, span }
            | ParserAst::GridRagged { child, span, .. }
            | ParserAst::Characters { child, span, .. } => {
                *span = span.shifted(delta);
                child.shift_spans(delta);
            }
            ParserAst::SectionsNamed {
                fields,
                repeated_tail,
                span,
            } => {
                *span = span.shifted(delta);
                for item in fields {
                    item.parser_mut().shift_spans(delta);
                }
                if let Some((_, tail)) = repeated_tail {
                    tail.shift_spans(delta);
                }
            }
            ParserAst::Block { items, span } => {
                *span = span.shifted(delta);
                for item in items {
                    match item {
                        BlockItem::Positional(p) | BlockItem::Named { parser: p, .. } => {
                            p.shift_spans(delta);
                        }
                    }
                }
            }
            ParserAst::Choice { cases, span } => {
                *span = span.shifted(delta);
                for (_, p) in cases {
                    p.shift_spans(delta);
                }
            }
        }
    }
}

/// Shift every span inside a template's parts by `delta` bytes.
///
/// Separate from [`ParserAst::shift_spans`] because the two callers need
/// different halves: the `Template` arm of `shift_spans` rebases the node's own
/// span *and* its parts, while [`crate::body`] — which has just scanned a
/// nested template whose interior has its own offsets — has to rebase the parts
/// **without** touching the enclosing span, which it already built in the outer
/// text's offsets. Doing that with an open-coded loop is how the nested case
/// came to be missed in the first place.
pub fn shift_part_spans(parts: &mut [TemplatePart], delta: u32) {
    for part in parts {
        match part {
            TemplatePart::Literal { span, .. } => *span = span.shifted(delta),
            TemplatePart::Capture {
                parser,
                span,
                name_span,
                ..
            } => {
                *span = span.shifted(delta);
                if let Some(n) = name_span {
                    *n = n.shifted(delta);
                }
                parser.shift_spans(delta);
            }
        }
    }
}

/// The name of a structural constructor — **the whole of §7.5**, not just the
/// M6 six.
///
/// This table used to know six names, and the eight M9 constructors were
/// dispatched ahead of it by an `if ctor_name == "…"` chain in `praxis-hir`
/// that took `args.into_iter().next()` and dropped the rest (IP-07). A
/// constructor with no row here had no arity, so it had no arity *error*
/// either: an unknown name became `None` with no diagnostic at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Constructor {
    Lines,
    Sections,
    Csv,
    Ws,
    Sep,
    Grid,
    Matrix,
    Chars,
    OneOf,
    Block,
    Choice,
    Optional,
    Scan,
    /// `repeated(P)` / `repeated(P, N)` — legal **only** as a named argument of
    /// a `sections` call (§7.5). It is in the table so that the name is known
    /// and its misuse is `MisplacedRepeatedTail` rather than "unknown
    /// constructor".
    Repeated,
}

/// The **shape** of a constructor call's argument list (§7.5).
///
/// A count was not enough: `sep` takes a string and then a parser, `choice`
/// takes named arguments and no positional ones, `chars` takes a parser and an
/// optional keyword. Checking only `positional_arity` is why
/// `optional(int, word)` and `choice(int)` both passed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgShape {
    /// Exactly `n` positional parsers and nothing else.
    Positional(usize),
    /// `sep("s", P)` — one string literal, then one parser.
    StringThenParser,
    /// `one_of("LR")` — one string literal.
    OneString,
    /// `chars(P, skip: policy)` — one parser and an optional `skip:` keyword.
    ParserWithSkip,
    /// `repeated(P)` or `repeated(P, N)` — one parser and an optional count
    /// literal. The count must be a literal because the parser plan is built
    /// when the program is compiled, so there is no runtime value in scope to
    /// read one from.
    ParserWithOptionalCount,
    /// `grid(P)` or `grid(P, ragged, fill: value)` — the ragged flag and the
    /// fill value come as a pair or not at all.
    GridMaybeRagged,
    /// `sections(P)` **or** `sections(name: P, …)` — the homogeneous and
    /// heterogeneous forms are one name with two shapes.
    OnePositionalOrNamed,
    /// `block(item, …)` — one or more positional parsers and/or named items.
    Items,
    /// `choice(Name: P, …)` — named arguments only, at least `at_least` of them.
    NamedOnly { at_least: usize },
}

impl Constructor {
    /// Parse a constructor name, or `None` if no §7.5 constructor is spelled
    /// that way.
    pub fn from_keyword(name: &str) -> Option<Self> {
        Some(match name {
            "lines" => Constructor::Lines,
            "sections" => Constructor::Sections,
            "csv" => Constructor::Csv,
            "ws" => Constructor::Ws,
            "sep" => Constructor::Sep,
            "grid" => Constructor::Grid,
            "matrix" => Constructor::Matrix,
            "chars" => Constructor::Chars,
            "one_of" => Constructor::OneOf,
            "block" => Constructor::Block,
            "choice" => Constructor::Choice,
            "optional" => Constructor::Optional,
            "scan" => Constructor::Scan,
            "repeated" => Constructor::Repeated,
            _ => return None,
        })
    }

    /// The source keyword for this constructor.
    pub fn keyword(self) -> &'static str {
        match self {
            Constructor::Lines => "lines",
            Constructor::Sections => "sections",
            Constructor::Csv => "csv",
            Constructor::Ws => "ws",
            Constructor::Sep => "sep",
            Constructor::Grid => "grid",
            Constructor::Matrix => "matrix",
            Constructor::Chars => "chars",
            Constructor::OneOf => "one_of",
            Constructor::Block => "block",
            Constructor::Choice => "choice",
            Constructor::Optional => "optional",
            Constructor::Scan => "scan",
            Constructor::Repeated => "repeated",
        }
    }

    /// Every constructor, so a test can sweep the table — and so the editor can
    /// offer them: completion, signature help and the parser keyword list read
    /// this, so a name missing here is a name the editor never offers.
    /// `constructor_round_trips_keywords_and_states_its_shape` is what keeps it
    /// complete.
    pub const ALL: &'static [Constructor] = &[
        Constructor::Lines,
        Constructor::Sections,
        Constructor::Csv,
        Constructor::Ws,
        Constructor::Sep,
        Constructor::Grid,
        Constructor::Matrix,
        Constructor::Chars,
        Constructor::OneOf,
        Constructor::Block,
        Constructor::Choice,
        Constructor::Optional,
        Constructor::Scan,
        Constructor::Repeated,
    ];

    /// One line of §7.5, for hover (§15.2's "method documentation", and its
    /// parser half).
    ///
    /// Exhaustive, and here rather than in the language server for the reason
    /// every other table is: a constructor added to §7.5 cannot ship without
    /// saying what it does, and the editor cannot describe one differently from
    /// the compiler. The wording is §7.5's own, compressed to a line.
    pub fn doc(self) -> &'static str {
        match self {
            Constructor::Lines => {
                "Split the region into lines and apply the parser to each. Every \
                 line must be consumed whole."
            }
            Constructor::Sections => {
                "Split the region on blank lines and apply the parser to each \
                 section. With named arguments, parses fixed sections in order \
                 into a record."
            }
            Constructor::Csv => {
                "Split the region on commas. Whitespace around a comma is \
                 forgiven, because the field's own parser does not read it."
            }
            Constructor::Ws => {
                "Split on runs of whitespace — line endings included, so a token \
                 never spans a line."
            }
            Constructor::Sep => "Split on an exact separator string, with no implicit trimming.",
            Constructor::Grid => {
                "Parse rectangular lines into a `Grid[T]`, one cell per parser \
                 application. `ragged` with `fill:` permits uneven rows."
            }
            Constructor::Matrix => {
                "Parse lines of whitespace-separated elements into a `Grid[T]`. \
                 Unlike `lines(ws(P))`, a row with no tokens is not a row."
            }
            Constructor::Chars => {
                "Apply a parser repeatedly to characters. `skip:` says what is \
                 passed over between matches: `none`, `whitespace`, `newlines`."
            }
            Constructor::OneOf => "Match one character from a literal set.",
            Constructor::Block => {
                "Apply parsers in sequence within one region. A positional item \
                 contributes its captures; a named one contributes a field."
            }
            Constructor::Choice => {
                "Parse one of several alternatives into an anonymous enum, one \
                 variant per named case."
            }
            Constructor::Optional => {
                "Return `Option[T]`. A failure consumes no input — this is \
                 parser-level optionality, not recovery."
            }
            Constructor::Scan => {
                "Find repeated matches inside otherwise irrelevant text, for \
                 input that embeds its data in noise."
            }
            Constructor::Repeated => {
                "A repeating group of sections in a heterogeneous `sections`. \
                 `repeated(P, N)` takes exactly N and may be followed; \
                 `repeated(P)` takes every section left, so it must be last."
            }
        }
    }

    /// The one named argument this constructor takes whose value is a
    /// **keyword and not a parser** — `chars(P, skip: policy)`'s `skip:` and
    /// `grid(P, ragged, fill: value)`'s `fill:` (§7.5). `None` for every other
    /// constructor.
    ///
    /// Both front ends used to decide this from the argument's *name* alone
    /// (`if name == "skip" || name == "fill"`), with no reference to the
    /// constructor being called. So a `block` item or a `sections` field
    /// legitimately named `fill` or `skip` was minted as a keyword argument,
    /// accepted by the shape check as a well-shaped named argument, and then
    /// dropped by a `filter_map` — the field vanished from the record with no
    /// diagnostic. A keyword belongs to a constructor, so the constructor is
    /// what answers the question.
    pub fn keyword_arg(self) -> Option<&'static str> {
        match self {
            Constructor::Chars => Some("skip"),
            Constructor::Grid => Some("fill"),
            _ => None,
        }
    }

    /// The one **bare keyword flag** this constructor takes — the `ragged` of
    /// `grid(P, ragged, fill: value)` (§7.5). `None` for every other
    /// constructor.
    ///
    /// The companion to [`Constructor::keyword_arg`], and here for the same
    /// reason: `ragged` had no row at all, so both front ends minted a
    /// `CallArg::Flag` from the bare *name* with no reference to the
    /// constructor being called. A bare `ragged` was therefore a flag in
    /// **every** constructor's argument list — `lines(ragged)` was told it had
    /// written a flag where a parser belongs rather than that `ragged` is not a
    /// parser, and the word was reserved everywhere instead of in `grid`. A
    /// flag belongs to a constructor, so the constructor is what answers the
    /// question.
    pub fn flag_arg(self) -> Option<&'static str> {
        match self {
            Constructor::Grid => Some("ragged"),
            _ => None,
        }
    }

    /// The shape of this constructor's argument list (§7.5).
    pub fn arg_shape(self) -> ArgShape {
        match self {
            Constructor::Lines
            | Constructor::Csv
            | Constructor::Ws
            | Constructor::Matrix
            | Constructor::Optional
            | Constructor::Scan => ArgShape::Positional(1),
            Constructor::Repeated => ArgShape::ParserWithOptionalCount,
            Constructor::Sections => ArgShape::OnePositionalOrNamed,
            Constructor::Sep => ArgShape::StringThenParser,
            Constructor::OneOf => ArgShape::OneString,
            Constructor::Chars => ArgShape::ParserWithSkip,
            Constructor::Grid => ArgShape::GridMaybeRagged,
            Constructor::Block => ArgShape::Items,
            Constructor::Choice => ArgShape::NamedOnly { at_least: 1 },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §7.4's list is a **closed set of ten**, and it used to be six: `uint`,
    /// `float`, `byte` and `identifier` had no row at all, so a program that
    /// wrote one of the design document's own atomic names got "unknown atomic
    /// parser" (IP-11).
    #[test]
    fn atomic_round_trips_keywords() {
        for kind in AtomicKind::ALL {
            assert_eq!(AtomicKind::from_keyword(kind.keyword()), Some(*kind));
            // Every §7.4 name is spelled here, exhaustively, for the reason
            // `Constructor`'s sweep below gives: the `names` assertion under
            // this loop looks like it pins the set, but it collects *from
            // `ALL`*, so an eleventh atomic left out of `ALL` leaves that list
            // ten long and green — the same blind spot `ALL.len() == 14` had.
            // Adding a variant fails to compile here instead.
            match kind {
                AtomicKind::Int
                | AtomicKind::UInt
                | AtomicKind::Float
                | AtomicKind::Byte
                | AtomicKind::Char
                | AtomicKind::Digit
                | AtomicKind::Word
                | AtomicKind::Identifier
                | AtomicKind::Text
                | AtomicKind::Rest => {}
            }
        }
        assert_eq!(AtomicKind::from_keyword("nope"), None);

        // §7.4 verbatim, in its own order.
        let names: Vec<&str> = AtomicKind::ALL.iter().map(|k| k.keyword()).collect();
        assert_eq!(
            names,
            vec![
                "int",
                "uint",
                "float",
                "byte",
                "char",
                "digit",
                "word",
                "identifier",
                "text",
                "rest"
            ]
        );
        // And nothing else is an atomic — an eleventh name would have to be
        // added to §7.4 first.
        for not_an_atomic in ["uint8", "integer", "string", "line", "lines", "sep"] {
            assert_eq!(AtomicKind::from_keyword(not_an_atomic), None);
        }
    }

    /// **Rewritten (IP-07).** This used to assert three numbers out of
    /// `expected_arity`, which was the whole of the constructor check — and a
    /// count cannot say that `sep`'s first argument is a *string*, that
    /// `choice` takes no positional argument at all, or that `optional` takes
    /// one and not two. Eight of §7.5's fourteen constructors had no row here,
    /// so they had no arity and therefore no arity error.
    ///
    /// The table now states the *shape*, and this asserts it for every name.
    #[test]
    fn constructor_round_trips_keywords_and_states_its_shape() {
        for ctor in Constructor::ALL {
            assert_eq!(
                Constructor::from_keyword(ctor.keyword()),
                Some(*ctor),
                "`{}` must round-trip through the table",
                ctor.keyword()
            );
            // Every §7.5 name is spelled here, exhaustively: adding a
            // constructor fails to compile at this match rather than passing
            // quietly out of `ALL`. That is what `ALL.len() == 14` could not
            // do — a count is green while the list is short and only fires
            // when the list *was* updated and the number was not. `ALL` feeds
            // completion, signature help and the keyword list as well as this
            // sweep, so a name missing from it is one the editor never offers.
            match ctor {
                Constructor::Lines
                | Constructor::Sections
                | Constructor::Csv
                | Constructor::Ws
                | Constructor::Sep
                | Constructor::Grid
                | Constructor::Matrix
                | Constructor::Chars
                | Constructor::OneOf
                | Constructor::Block
                | Constructor::Choice
                | Constructor::Optional
                | Constructor::Scan
                | Constructor::Repeated => {}
            }
        }
        assert_eq!(Constructor::from_keyword("frobnicate"), None);

        // And a constructor cannot be added without deciding what its arguments
        // look like.
        assert_eq!(Constructor::Lines.arg_shape(), ArgShape::Positional(1));
        assert_eq!(Constructor::Optional.arg_shape(), ArgShape::Positional(1));
        assert_eq!(Constructor::Sep.arg_shape(), ArgShape::StringThenParser);
        assert_eq!(Constructor::OneOf.arg_shape(), ArgShape::OneString);
        assert_eq!(Constructor::Chars.arg_shape(), ArgShape::ParserWithSkip);
        assert_eq!(Constructor::Grid.arg_shape(), ArgShape::GridMaybeRagged);
        assert_eq!(
            Constructor::Sections.arg_shape(),
            ArgShape::OnePositionalOrNamed
        );
        assert_eq!(Constructor::Block.arg_shape(), ArgShape::Items);
        assert_eq!(
            Constructor::Choice.arg_shape(),
            ArgShape::NamedOnly { at_least: 1 }
        );
        assert_eq!(
            Constructor::Repeated.arg_shape(),
            ArgShape::ParserWithOptionalCount
        );

        // And which constructor owns `ragged` — one, and not "whichever call
        // happens to have a bare `ragged` in it", which is what both front ends
        // used to answer.
        for ctor in Constructor::ALL {
            let expected = (*ctor == Constructor::Grid).then_some("ragged");
            assert_eq!(ctor.flag_arg(), expected, "`{}`", ctor.keyword());
        }
    }

    /// **The count that names no sections is not constructible.**
    ///
    /// `repeated(P, 0)` would consume nothing and produce an empty `Vec`, which
    /// is a parser nobody writes on purpose and reads as a typo for the
    /// unbounded form; a negative count names no sections at all. A `validate`
    /// arm would catch either only where somebody remembered to call it, so the
    /// one constructor refuses them — the same argument `Separator` makes about
    /// the separator that never advances.
    #[test]
    fn a_repeat_count_is_positive_by_construction() {
        assert_eq!(RepeatCount::new(0), Err(InvalidRepeatCount::NotPositive));
        assert_eq!(RepeatCount::new(-3), Err(InvalidRepeatCount::NotPositive));
        assert_eq!(
            RepeatCount::new(1 << 33),
            Err(InvalidRepeatCount::TooLarge),
            "the plan node holds a u32, so the refusal happens where the span is"
        );

        assert_eq!(RepeatCount::new(1).expect("one section").get(), 1);
        assert_eq!(RepeatCount::new(6).expect("six sections").get(), 6);
        assert_eq!(
            RepeatCount::new(i64::from(u32::MAX))
                .expect("the largest count the plan can hold")
                .get(),
            u32::MAX
        );
    }

    /// A counted item wants its count's worth of sections and a plain one wants
    /// exactly one — the number the runtime's cursor advances by, stated once
    /// here so the walk and the shortfall diagnostic cannot disagree about it.
    #[test]
    fn a_section_items_appetite_is_its_count() {
        let atom = || ParserAst::Atomic {
            kind: AtomicKind::Int,
            span: Span::at(0),
        };
        let one = SectionItem::One {
            name: "regions".to_string(),
            parser: atom(),
        };
        let counted = SectionItem::Counted {
            name: "shapes".to_string(),
            count: RepeatCount::new(6).expect("six sections"),
            parser: atom(),
        };
        assert_eq!(one.sections_wanted(), 1);
        assert_eq!(counted.sections_wanted(), 6);
        assert_eq!(one.name(), "regions");
        assert_eq!(counted.name(), "shapes");
    }
}
