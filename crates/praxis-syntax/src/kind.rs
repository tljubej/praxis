//! The single vocabulary of Praxis syntax: tokens, trivia, and tree nodes.
//!
//! `SyntaxKind` is one enumeration that carries every leaf token the lexer
//! emits (literals, keywords, operators, trivia) *and* every interior node the
//! parser produces. This is the rowan idiom (ADR-003): one `#[repr(u16)]` enum
//! backs a strongly-typed lossless tree, and the [`PraxisLanguage`](crate::PraxisLanguage)
//! implementation carries it through `rowan`'s generic node types.
//!
//! Adding a construct is therefore two edits: a token kind (if it is a new
//! leaf) or a node kind, plus the parser code that emits it. The kinds are kept
//! exhaustive here so the lexer and parser never need to invent identifiers at
//! runtime — illegal kinds are unrepresentable.

// `is_token`/`is_node`/keyword tables are exercised by the unit tests below;
// the large match arms are exhaustive by construction.

#![allow(dead_code)] // the kind space is exhaustive; not every kind has a consumer.

/// Every lexical token, piece of trivia, and tree node in Praxis.
///
/// The ordering inside the enum is grouping-only (comments delimit the
/// sections) and carries no semantic meaning. The discriminants are stable
/// `u16` values because rowan stores them as raw integers in the green tree.
///
/// Naming convention: keywords carry a `KW_` prefix, punctuation a prefix
/// matching its role (`L_`/`R_` for matching pairs), and tree nodes an `_EXPR`/
/// `_STMT`/`_ITEM` suffix. The screaming-snake names make lexical kinds visually
/// distinct from the CamelCase AST wrappers in `praxis-ast`, which is why we
/// relax the usual camel-case lint for this one enum.
#[repr(u16)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[allow(non_camel_case_types)]
pub enum SyntaxKind {
    // ---- Trivia (kept in the lossless tree, ignored for parsing) ----
    /// A run of spaces, tabs, and newlines outside a comment.
    Whitespace,
    /// A `//` line comment (not including the trailing newline).
    LineComment,
    /// A nestable `/* ... */` block comment, including delimiters.
    BlockComment,

    // ---- Identifiers and literals ----
    /// An identifier that is *not* a keyword.
    Ident,
    /// An integer literal, e.g. `42`.
    IntLit,
    /// A floating-point literal, e.g. `3.14`, `1e10`, `.5`, `2.` (§4.12).
    /// A bare `.` is `DOT`; a float literal needs a digit on at least one side
    /// of the dot, or an exponent. A `.` immediately followed by another `.` is
    /// a range (`..` / `..=`), never part of a float.
    FloatLit,
    /// A double-quoted text literal with **no interpolation holes**, e.g.
    /// `"hello"` — the whole literal, quotes included.
    ///
    /// A literal that holds a `{` is not this kind: it is an
    /// [`InterpOpen`](Self::InterpOpen) / [`InterpMiddle`](Self::InterpMiddle) /
    /// [`InterpClose`](Self::InterpClose) run with the holes' ordinary tokens
    /// between the fragments (§8.1, ADR-147). **An unterminated literal is this
    /// kind either way**, holes or not: the lexer only splits a literal it has
    /// already proved closes on its line, so `T004` reports the whole run as one
    /// token.
    TextLit,
    /// The first fragment of an interpolated text literal: the opening `"`, the
    /// literal text before the first hole, and the `{` that opens it — e.g.
    /// `"Part 2: {` (§8.1, ADR-147).
    ///
    /// The delimiters are *inside* the token, one byte at each end, so the three
    /// fragment kinds decode identically (`&text[1..len-1]` through
    /// [`praxis_syntax::literal::decode_text_body`]) and the token stream still
    /// tiles the source (ADR-003).
    ///
    /// The fragments are separate tokens rather than one opaque literal because
    /// a name inside a hole has to be a **token at its own range** in the
    /// lossless tree: that is the only way `praxis-hir`'s capture analysis,
    /// which looks token ranges up in the resolver's map, sees it. A closure
    /// body of `"{outer}"` would otherwise capture nothing and read a slot
    /// nothing filled (ADR-147 decision 1).
    ///
    /// [`praxis_syntax::literal::decode_text_body`]: crate::literal::decode_text_body
    InterpOpen,
    /// A fragment between two holes: the `}` closing one, the literal text, and
    /// the `{` opening the next — e.g. `} and {`. Empty text is ordinary
    /// (`"{a}{b}"` has the two-byte fragment `}{`).
    InterpMiddle,
    /// The last fragment: the `}` closing the final hole, the trailing literal
    /// text, and the closing `"` — e.g. `}!"`.
    InterpClose,
    /// A single-quoted character literal, e.g. `'#'` (ADR-141).
    ///
    /// Exactly **one** Unicode scalar, and the lexer is where that is decided:
    /// `''` and `'ab'` are `T007`, not a silently truncated `Char`. Its escapes
    /// are the text literal's — `\n \r \t \0 \\ \"` — plus `\'`, and there are
    /// no `\x`/`\u{…}` forms, because two escape tables for one language is the
    /// drift `praxis_syntax::literal`'s module doc was written to forbid.
    ///
    /// **This kind means the literal closed.** An unterminated run is still
    /// pushed as a `CharLit` (losslessness, ADR-003) after a `T006`, so a
    /// consumer must ask [`praxis_syntax::literal::decode_char_literal`] rather
    /// than assume; there is no second kind here the way there is for a
    /// template, because a `'` cannot open a sublanguage nobody scanned.
    CharLit,
    /// A backtick-delimited parser template, e.g. `` `{x:int}` ``. The whole
    /// template is one token; its interior is re-scanned by the input-parser
    /// lexer (§7).
    ///
    /// **This kind means the template closed.** A run that did not is
    /// [`SyntaxKind::UnterminatedBacktickTemplate`], so a `BacktickTemplate`'s
    /// text is a complete template *by construction* and no consumer has to
    /// re-derive that (ADR-094).
    BacktickTemplate,
    /// A backtick run that did not close before its line ended (ADR-094).
    ///
    /// Two kinds rather than one predicate, because "is this token terminated"
    /// must not be re-derived by each consumer. A template ends at its line, so
    /// the common unterminated token *is* `` `{int` ``: a hand-rolled
    /// `strip_prefix('`').and_then(strip_suffix('`'))` succeeds on it, the
    /// interior scanner is handed `{int`, and `I030` comes back describing an
    /// interior nobody wrote — the fabricated-interior class that
    /// `an_unterminated_template_does_not_also_report_a_fabricated_interior`
    /// exists to forbid.
    ///
    /// So the state is made unrepresentable instead: the lexer decides once,
    /// and a consumer that receives this kind knows there is nothing to scan.
    /// It also means such a token can be typed with a fresh variable rather than
    /// drawing `Y023` ("write `read` before it") — advice that cannot close a
    /// template.
    UnterminatedBacktickTemplate,

    // ---- Keywords (§4) ----
    KW_VAR,      // `var`
    KW_FN,       // `fn`
    KW_IF,       // `if`
    KW_ELSE,     // `else`
    KW_WHILE,    // `while`
    KW_FOR,      // `for`
    KW_IN,       // `in` (for-loop iterator separator, §4.11)
    KW_LOOP,     // `loop`
    KW_MATCH,    // `match`
    KW_RETURN,   // `return`
    KW_BREAK,    // `break`
    KW_CONTINUE, // `continue`
    KW_READ,     // `read`
    KW_STRUCT,   // `struct`
    KW_ENUM,     // `enum`
    KW_TRUE,     // `true`
    KW_FALSE,    // `false`

    // ---- Punctuation and operators ----
    /// `(`
    L_PAREN,
    /// `)`
    R_PAREN,
    /// `{`
    L_BRACE,
    /// `}`
    R_BRACE,
    /// `[`
    L_BRACK,
    /// `]`
    R_BRACK,
    /// `,`
    COMMA,
    /// `.`
    DOT,
    /// `..`
    DOT2,
    /// `..=`
    DOT2EQ,
    /// `:`
    COLON,
    /// `;`
    SEMICOLON,
    /// `->`
    THIN_ARROW,
    /// `=>`
    FAT_ARROW,
    /// `#`
    HASH,
    /// `|`
    PIPE,
    /// `||`
    PIPE2,
    /// `&`
    AMP,
    /// `&&` — logical and. The lexer's max-munch keeps it one token, as it does
    /// `||`, so a bare `AMP` is never part of one.
    AMP2,
    /// `_` — a lone underscore (placeholder/punning site).
    UNDERSCORE,

    // Arithmetic operators.
    /// `+`
    PLUS,
    /// `-`
    MINUS,
    /// `*`
    STAR,
    /// `/`
    SLASH,
    /// `%`
    PERCENT,

    // Compound-assignment operators.
    /// `+=`
    PLUS_EQ,
    /// `-=`
    MINUS_EQ,
    /// `*=`
    STAR_EQ,
    /// `/=`
    SLASH_EQ,
    /// `%=`
    PERCENT_EQ,

    // Comparison operators.
    /// `==`
    EQ2,
    /// `!=`
    NEQ,
    /// `<`
    LT,
    /// `>`
    GT,
    /// `<=`
    LTEQ,
    /// `>=`
    GTEQ,

    /// `=` (assignment / binding).
    EQ,
    /// `!` (logical not).
    BANG,
    /// `?` (reserved for later use).
    QUESTION,

    // ---- Sentinel ----
    /// End of input. Emitted as the final token so the parser can treat EOF
    /// uniformly.
    EOF,
    /// A byte the lexer does not recognize. The lexer also emits a real
    /// diagnostic (`T003`) for it rather than silently dropping it.
    ERROR,

    // ---- Tree nodes (produced by the parser) ----
    /// The root node of a parsed file.
    SOURCE_FILE,
    /// A `var name = expr` binding — the language's one binding form (ADR-125).
    VAR_STMT,
    /// A bare expression used as a statement.
    EXPR_STMT,
    /// A reassignment statement: `name = expr` or `name += expr` etc. (§4.2).
    ASSIGN_STMT,
    /// A reassignment through a place expression: `m[key] = expr`,
    /// `counts[key] += 1` (§6.2).
    ///
    /// Its own kind rather than an `ASSIGN_STMT` with an expression target: an
    /// `ASSIGN_STMT`'s target is a *token* and its single expression child is the
    /// value, so a target that is itself an expression cannot be told from the
    /// value. The target here is the first expression child and the value the
    /// second.
    PLACE_ASSIGN_STMT,
    /// The two-token `min=` / `max=` operator of an updating store (§6.2): an
    /// `Ident` spelling `min` or `max`, immediately followed by `=`.
    ///
    /// A node rather than a token because `min` **is** an identifier — the lexer
    /// cannot claim it without taking `min` away from every program that names
    /// the prelude helper — so the operator is decided contextually, at the one
    /// position where an identifier cannot otherwise appear. Wrapping the pair
    /// keeps the `=` from being a direct child of the statement, where a walk
    /// looking for the assignment operator would read the update as a plain
    /// store.
    UPDATE_OP,
    /// The two-token `:bp` marker a statement may end with (§9.8): a `COLON`
    /// immediately followed by an `Ident` spelling `bp`.
    ///
    /// A node rather than a token for [`UPDATE_OP`](Self::UPDATE_OP)'s reason,
    /// and the same reason it is decided by *position* instead of by the lexer:
    /// `bp` is an identifier everywhere else, and a lexer rule claiming `:bp`
    /// would take `bp` away from every program that annotates a binding with a
    /// type whose name begins that way. The one place an identifier cannot
    /// otherwise follow a `:` is the end of a statement, which is exactly where
    /// this is admitted. Wrapping the pair keeps the `:` from being a direct
    /// child of the statement, where a walk looking for a type annotation would
    /// find it.
    BREAKPOINT,
    /// A top-level or nested `fn` declaration.
    FN_ITEM,
    /// A `struct Name { field: Type, … }` declaration (§4.5).
    STRUCT_ITEM,
    /// An `enum Name { Variant, Variant(Type), … }` declaration (§4.6).
    ENUM_ITEM,
    /// One variant of an enum: `Name` or `Name(Type, …)`.
    ENUM_VARIANT,
    /// The `{ field: Type, … }` body of a struct declaration.
    FIELD_LIST,
    /// A single `name: Type` field of a struct.
    FIELD,
    /// A `Name { field: expr, … }` record-literal expression (§4.5).
    RECORD_LIT_EXPR,
    /// A `receiver.0` tuple-element expression (§4.4).
    ///
    /// Its own kind rather than a `FIELD_EXPR` holding an `IntLit`: an element is
    /// selected by **position** and the index must be a literal, where a field is
    /// selected by name — two different operations that lower to two different
    /// runtime calls.
    TUPLE_INDEX_EXPR,
    /// The `[Type, …]` type-argument list of a constructor call (§3.3):
    /// the brackets in `Counter[(Int, Int)]()`.
    ///
    /// Its own kind rather than an `INDEX_EXPR` holding types: the brackets in
    /// `Counter[(Int, Int)]()` and in `m[key]` are the same two characters and
    /// two different operations, and only the *name* in front tells them apart
    /// (`Int` is a legal expression too, so the contents cannot).
    TYPE_ARG_LIST,
    /// A `receiver[index]` subscript expression (§4.7/§6.2/§6.4).
    ///
    /// The index list is an `ARG_LIST`, because §6.4's `grid[x, y]` makes a
    /// subscript variadic: the arity is part of what selects the operation, the
    /// same way a method call's is.
    INDEX_EXPR,
    /// A `receiver.field` field-access expression (§4.5).
    FIELD_EXPR,
    /// A `match scrutinee { pattern => expr, … }` expression (§4.6/§4.11).
    MATCH_EXPR,
    /// A closure expression `|params| expr` (§4.10). Bare `PIPE` claims the
    /// `|` (lexer max-munch keeps `||` as logical-or `PIPE2`).
    CLOSURE_EXPR,
    /// One `pattern => expr` arm of a match expression.
    MATCH_ARM,
    /// A pattern (§4.6): wildcard `_`, literal, variable bind, enum variant,
    /// or tuple/record destructuring.
    PATTERN,
    /// One `name` or `name: pattern` field of a record pattern (§4.5).
    ///
    /// Its own kind rather than the [`FIELD`](Self::FIELD) a struct declaration
    /// and a record literal share: those hold a type and an expression, and this
    /// holds a *pattern*. A punned `P { x }` and an explicit `P { x: p }` are
    /// then one node shape — the name is always the token, the sub-pattern is
    /// always the optional child — so pairing a field with its pattern never has
    /// to count identifiers.
    PATTERN_FIELD,
    /// A single `name: Type` parameter.
    PARAM,
    /// The `(...)` parameter list.
    PARAM_LIST,
    /// A `{ ... }` block expression.
    BLOCK_EXPR,
    /// An `if cond { ... } else { ... }` expression.
    IF_EXPR,
    /// The `else` arm (block or `else if`).
    ELSE_BRANCH,
    /// A `while cond { ... }` expression.
    WHILE_EXPR,
    /// A `for pat in iter { ... }` expression (§4.11).
    FOR_EXPR,
    /// A `loop { ... }` expression (§4.11).
    LOOP_EXPR,
    /// A `break [expr]` expression (§4.11).
    BREAK_EXPR,
    /// A `continue` expression (§4.11).
    CONTINUE_EXPR,
    /// A `return [expr]` expression (§4.11).
    RETURN_EXPR,
    /// A `callee(args)` call expression (covers `out(...)`).
    CALL_EXPR,
    /// A `receiver.method(args)` method-call expression (§16.2).
    METHOD_CALL_EXPR,
    /// The `(arg, arg, ...)` argument list of a call.
    ARG_LIST,
    /// A path: an identifier or a dotted name.
    PATH_EXPR,
    /// A literal value
    /// (`IntLit`/`FloatLit`/`TextLit`/`CharLit`/`true`/`false`/backtick
    /// template).
    LITERAL,
    /// An interpolated text literal: `"a{x}b"` (§8.1, ADR-147).
    ///
    /// Its children alternate — [`InterpOpen`](Self::InterpOpen), an expression,
    /// then zero or more [`InterpMiddle`](Self::InterpMiddle)/expression pairs,
    /// then [`InterpClose`](Self::InterpClose) — and the expressions are
    /// ordinary expression subtrees, not a sublanguage.
    ///
    /// Its own kind rather than a [`LITERAL`](Self::LITERAL) with children,
    /// because it is not one: a `LITERAL` is a leaf whose value the lowerer
    /// reads off a token, and every walk in the workspace that finds names,
    /// resolves them, renames them or captures them has to descend into a hole.
    /// Giving `LITERAL` children would have made "does this node contain a name"
    /// a question with two answers.
    INTERP_EXPR,
    /// A reference to a name (identifier used as a value).
    NAME_REF,
    /// A binary operator expression, e.g. `a + b`.
    BIN_EXPR,
    /// A range expression: `a..b` (half-open) or `a..=b` (inclusive) — §4.11,
    /// ADR-059. Its own node kind rather than a [`BIN_EXPR`](Self::BIN_EXPR):
    /// a range is not an operator applied to two numbers, it is a *collection*
    /// built from two bounds, and every consumer that asks "what binary
    /// operator is this" would otherwise have to answer "none of them".
    RANGE_EXPR,
    /// A unary operator expression, e.g. `-x`.
    UNARY_EXPR,
    /// A parenthesized expression `( expr )`.
    PAREN_EXPR,
    /// A tuple expression `( e1, e2, … )` with two or more elements. A
    /// single parenthesized value is [`PAREN_EXPR`](Self::PAREN_EXPR), not this.
    TUPLE_EXPR,
    /// A list expression `[ e1, e2, … ]` — a `Vec` literal (§6.1).
    ///
    /// Its own kind rather than an [`INDEX_EXPR`](Self::INDEX_EXPR) with no
    /// receiver: the brackets in `[1, 2]` and in `m[k]` are the same two
    /// characters and two different operations, and what tells them apart is
    /// **position** — a subscript continues an expression, a list begins one.
    /// That is the rule [`TYPE_ARG_LIST`](Self::TYPE_ARG_LIST) is decided by, and
    /// the rule that decides the `(` too.
    LIST_EXPR,
    /// A type written in source: a scalar or grouped type name (`Int`, `Text`, …),
    /// with or without a bracketed type-argument list (§4.4). Tuple and
    /// function types carry their own kinds.
    TYPE_REF,
    /// A tuple type `(T, U, …)`. A parenthesized single type `(T)` is just `T`,
    /// so this always carries two or more elements.
    TUPLE_TYPE,
    /// A function type `(P0, P1, …) -> R`.
    FN_TYPE,
    /// A parse-error placeholder node wrapping tokens the parser could not
    /// place. Recovery (§15.2) emits these so the tree stays well-formed.
    PARSE_ERROR,
    // ---- Input-parser expression nodes (§7) ----
    /// `read parser_expression` — a prefix expression applying a parser to the
    /// whole process-input buffer (§7.1).
    READ_EXPR,
    /// `parse(text, parser_expression)` — apply a parser to an existing `Text`
    /// value (§7.1).
    PARSE_EXPR,
    /// A parser expression (§7 EBNF): an atomic, a template, or a constructor
    /// call. The body of `read` and the second arg of `parse`.
    PARSER_EXPR,
    /// An atomic parser name: `int`, `char`, `word`, etc. (§7.4).
    PARSER_ATOM,
    /// A backtick template `` `{x:int},{y:int}` `` inside a parser expression
    /// (§7.2). Its children are the scanned template parts.
    PARSER_TEMPLATE,
    /// A `{name:parser}` or `{parser}` capture inside a template (§7.3).
    PARSER_CAPTURE,
    /// A constructor call `lines(P)`, `csv(P)`, `sep(sep, P)`, etc. (§7.5).
    PARSER_CALL,
    /// The `(arg, arg, ...)` argument list of a parser constructor call.
    PARSER_ARG_LIST,
    /// A named argument inside a parser constructor call (§7.5):
    /// `name: parser_expr`, e.g. `rules: lines(int)` in heterogeneous
    /// `sections`, or `skip: whitespace` in `chars`. Holds the name ident, the
    /// `:`, and the parser-expr value.
    PARSER_NAMED_ARG,
    /// The **literal** value of a keyword argument inside a parser constructor
    /// call: the `0` of `grid(char, ragged, fill: 0)` or the `"-"` of
    /// `fill: "-"` (§7.5).
    ///
    /// Its own kind because a keyword argument's value is not a parser
    /// expression and cannot be parsed as one: handing it to `parse_parser_expr`
    /// reports `P001 expected a parser expression` and leaves a `PARSE_ERROR`
    /// with no literal for the HIR bridge to read, so §7.5's own documented
    /// spelling would build a ragged grid padded with `""` instead of `0`.
    PARSER_KEYWORD_VALUE,
}

impl SyntaxKind {
    /// Whether this kind is trivia: whitespace or a comment. Trivia is kept in
    /// the lossless tree (§13.1) but skipped for parsing decisions.
    #[must_use]
    pub fn is_trivia(self) -> bool {
        matches!(
            self,
            Self::Whitespace | Self::LineComment | Self::BlockComment
        )
    }

    /// Whether this kind is a keyword token.
    ///
    /// Derived from [`SyntaxKind::keyword_text`] rather than maintained as a
    /// second list, so a kind cannot be a keyword in one table and not in the
    /// other.
    #[must_use]
    pub fn is_keyword(self) -> bool {
        self.keyword_text().is_some()
    }

    /// Every keyword's source spelling, in discriminant order.
    ///
    /// **Swept, not listed.** The whole kind space is walked and filtered by
    /// [`is_keyword`](Self::is_keyword), so a keyword added to
    /// [`keyword_text`](Self::keyword_text) joins this by construction.
    ///
    /// The TextMate grammar is tested against this: the editor's keyword
    /// pattern is a copy of the lexer's table that no compiler checks, and the
    /// failure — a word quietly stopping being coloured — is one nobody files.
    #[must_use]
    pub fn all_keyword_texts() -> Vec<&'static str> {
        (0..=Self::LAST)
            .map(Self::from_raw_u16)
            .filter_map(Self::keyword_text)
            .collect()
    }

    /// Whether this kind is one of the three shapes a written type annotation
    /// can take: a name (with or without bracketed arguments), a tuple, or a
    /// function type.
    ///
    /// The set lives here, once, because everything that looks at an annotation
    /// needs the same answer: `praxis_ast::TypeRef::cast` accepts exactly these
    /// kinds, and type resolution recurses through exactly these children. A
    /// site that spelled the list out for itself and listed only `TYPE_REF`
    /// would silently drop every direct tuple and function annotation.
    #[must_use]
    pub fn is_type_node(self) -> bool {
        matches!(self, Self::TYPE_REF | Self::TUPLE_TYPE | Self::FN_TYPE)
    }

    /// Whether this kind is a token the parser wraps in a
    /// [`LITERAL`](Self::LITERAL) node: the four scalar literals, `true`/`false`,
    /// and both backtick-template kinds.
    ///
    /// Here for [`is_type_node`](Self::is_type_node)'s reason: the parser writes
    /// this set when it builds the node and `praxis_ast::Literal::token` reads
    /// it back. Two copies drift, and a reader missing a kind answers `None` for
    /// a `LITERAL` the parser really did build, dropping every HIR pass into its
    /// "no token at all" branch.
    ///
    /// **Both template kinds are in.** A template in value position has no
    /// meaning — §7.1 enters the parser sublanguage at `read`/`parse` and nowhere
    /// else — and is reported as `Y023`, but it is reported *about the token*,
    /// and an accessor that cannot see the token cannot report on it. The
    /// unterminated one draws no `Y023`, since that advice cannot close a
    /// template (ADR-094); it types as a fresh variable, which is exactly what
    /// the missing-token branch happened to produce.
    ///
    /// `true`/`false` are literals too, and take the same parse arm: an arm of
    /// their own that did not eat leading trivia first would make `true` span
    /// `" true"` where `1` spans `"1"`.
    #[must_use]
    pub fn is_literal_token(self) -> bool {
        matches!(
            self,
            Self::IntLit
                | Self::FloatLit
                | Self::TextLit
                | Self::CharLit
                | Self::BacktickTemplate
                | Self::UnterminatedBacktickTemplate
                | Self::KW_TRUE
                | Self::KW_FALSE
        )
    }

    /// Whether this kind is a literal a **pattern** may test against (§4.6): an
    /// integer, text, a character, `true` or `false`.
    ///
    /// Strictly narrower than [`is_literal_token`](Self::is_literal_token), and
    /// the difference is that a pattern tests a *constant*. There is no float
    /// pattern (§4.6), and a backtick template is not a constant either — nor is
    /// an interpolated literal, which the parser refuses in pattern position
    /// outright (ADR-147): `match s { "{x}" => … }` would otherwise leave a
    /// pattern whose only direct `Ident` is the hole's `x`, read as a variable
    /// bind, and swallow every value.
    ///
    /// `CharLit` is in the set (ADR-141). A caller's copy of this list that
    /// omitted it would stop a `match` arm list after `'#' => …`, dropping every
    /// arm below it from the tree with no diagnostic at all.
    #[must_use]
    pub fn is_pattern_literal(self) -> bool {
        matches!(
            self,
            Self::IntLit | Self::TextLit | Self::CharLit | Self::KW_TRUE | Self::KW_FALSE
        )
    }

    /// The largest discriminant. Sound because the enum declares no explicit
    /// discriminants, so its values are consecutive from zero — which
    /// [`SyntaxKind::from_raw_u16`] relies on and
    /// `every_raw_value_in_range_round_trips` checks.
    const LAST: u16 = SyntaxKind::PARSER_KEYWORD_VALUE as u16;

    /// Total conversion from a raw `u16`. Out-of-range values become
    /// [`SyntaxKind::ERROR`] — the safe rowan `Language` boundary must never
    /// construct an invalid enum discriminant, whatever the input.
    #[must_use]
    pub const fn from_raw_u16(raw: u16) -> SyntaxKind {
        if raw > Self::LAST {
            return SyntaxKind::ERROR;
        }
        // SAFETY: `SyntaxKind` is `#[repr(u16)]` with no explicit
        // discriminants, so 0..=LAST are exactly its valid values, and `raw` is
        // checked to be in that range.
        unsafe { std::mem::transmute::<u16, SyntaxKind>(raw) }
    }

    /// Whether this kind is a leaf token (emitted by the lexer), as opposed to
    /// trivia or an interior tree node.
    #[must_use]
    pub fn is_token(self) -> bool {
        !self.is_trivia() && !self.is_node()
    }

    /// Whether this kind is an interior tree node (produced by the parser).
    #[must_use]
    pub fn is_node(self) -> bool {
        self >= Self::SOURCE_FILE
    }

    /// Look up the keyword kind for an identifier's text, or `None` if it is a
    /// plain identifier. Used by the lexer to split keywords out of the ident
    /// run via a single table.
    #[must_use]
    pub fn from_keyword(text: &str) -> Option<SyntaxKind> {
        Some(match text {
            "var" => Self::KW_VAR,
            "fn" => Self::KW_FN,
            "if" => Self::KW_IF,
            "else" => Self::KW_ELSE,
            "while" => Self::KW_WHILE,
            "for" => Self::KW_FOR,
            "in" => Self::KW_IN,
            "loop" => Self::KW_LOOP,
            "match" => Self::KW_MATCH,
            "return" => Self::KW_RETURN,
            "break" => Self::KW_BREAK,
            "continue" => Self::KW_CONTINUE,
            "read" => Self::KW_READ,
            "struct" => Self::KW_STRUCT,
            "enum" => Self::KW_ENUM,
            "true" => Self::KW_TRUE,
            "false" => Self::KW_FALSE,
            _ => return None,
        })
    }

    /// The source spelling of a keyword, or `None` for non-keywords. The
    /// inverse of [`from_keyword`]; handy for diagnostics and completion, which
    /// have to spell a keyword back out.
    #[must_use]
    pub fn keyword_text(self) -> Option<&'static str> {
        Some(match self {
            Self::KW_VAR => "var",
            Self::KW_FN => "fn",
            Self::KW_IF => "if",
            Self::KW_ELSE => "else",
            Self::KW_WHILE => "while",
            Self::KW_FOR => "for",
            Self::KW_IN => "in",
            Self::KW_LOOP => "loop",
            Self::KW_MATCH => "match",
            Self::KW_RETURN => "return",
            Self::KW_BREAK => "break",
            Self::KW_CONTINUE => "continue",
            Self::KW_READ => "read",
            Self::KW_STRUCT => "struct",
            Self::KW_ENUM => "enum",
            Self::KW_TRUE => "true",
            Self::KW_FALSE => "false",
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trivia_classification() {
        assert!(SyntaxKind::Whitespace.is_trivia());
        assert!(SyntaxKind::LineComment.is_trivia());
        assert!(SyntaxKind::BlockComment.is_trivia());
        assert!(!SyntaxKind::Ident.is_trivia());
        assert!(!SyntaxKind::KW_IF.is_trivia());
        assert!(!SyntaxKind::PLUS.is_trivia());
    }

    #[test]
    fn token_vs_node_partition() {
        // Tokens and trivia are not nodes; everything from SOURCE_FILE up is.
        assert!(SyntaxKind::Ident.is_token());
        assert!(SyntaxKind::IntLit.is_token());
        // A newly inserted token kind lands *before* `SOURCE_FILE` or it is
        // silently reclassified: `is_node` is `self >= SOURCE_FILE`, so the
        // partition is decided by declaration order and nothing else says so.
        assert!(SyntaxKind::CharLit.is_token());
        assert!(!SyntaxKind::CharLit.is_node());
        assert!(SyntaxKind::KW_VAR.is_token());
        assert!(SyntaxKind::PLUS.is_token());
        assert!(SyntaxKind::EOF.is_token());
        assert!(!SyntaxKind::Whitespace.is_token()); // trivia, not a token
        assert!(!SyntaxKind::VAR_STMT.is_token()); // node
        assert!(SyntaxKind::SOURCE_FILE.is_node());
        assert!(SyntaxKind::PARSE_ERROR.is_node());
        assert!(!SyntaxKind::Ident.is_node());
        assert!(!SyntaxKind::EOF.is_node());
    }

    #[test]
    fn every_pattern_literal_is_a_literal_token() {
        // Swept, not listed: the pattern set is a strict subset of the literal
        // set, so a kind added to one and forgotten in the other is caught here
        // rather than by a `Literal::token` that quietly answers `None`.
        for raw in 0..=SyntaxKind::LAST {
            let kind = SyntaxKind::from_raw_u16(raw);
            assert!(
                !kind.is_pattern_literal() || kind.is_literal_token(),
                "{kind:?} tests as a pattern literal but is not a literal token"
            );
        }
        // The three the pattern grammar leaves out, each for the doc's reason.
        assert!(!SyntaxKind::FloatLit.is_pattern_literal()); // §4.6: no float pattern
        assert!(!SyntaxKind::BacktickTemplate.is_pattern_literal());
        assert!(!SyntaxKind::UnterminatedBacktickTemplate.is_pattern_literal());
        // …and the two that must be in their sets.
        assert!(SyntaxKind::CharLit.is_pattern_literal()); // ADR-141
        assert!(SyntaxKind::UnterminatedBacktickTemplate.is_literal_token()); // ADR-094
        assert!(!SyntaxKind::Ident.is_literal_token());
        assert!(!SyntaxKind::UNDERSCORE.is_pattern_literal());
    }

    #[test]
    fn keyword_round_trip() {
        // Every keyword round-trips through from_keyword/keyword_text.
        let all = [
            SyntaxKind::KW_VAR,
            SyntaxKind::KW_FN,
            SyntaxKind::KW_IF,
            SyntaxKind::KW_ELSE,
            SyntaxKind::KW_WHILE,
            SyntaxKind::KW_FOR,
            SyntaxKind::KW_LOOP,
            SyntaxKind::KW_MATCH,
            SyntaxKind::KW_RETURN,
            SyntaxKind::KW_BREAK,
            SyntaxKind::KW_CONTINUE,
            SyntaxKind::KW_READ,
            SyntaxKind::KW_STRUCT,
            SyntaxKind::KW_ENUM,
            SyntaxKind::KW_TRUE,
            SyntaxKind::KW_FALSE,
        ];
        for kw in all {
            assert!(kw.is_keyword());
            let text = kw.keyword_text().expect("keyword has text");
            assert_eq!(SyntaxKind::from_keyword(text), Some(kw), "{text}");
        }
    }

    #[test]
    fn regression_in_is_classified_consistently_with_the_keyword_table() {
        let kind = SyntaxKind::from_keyword("in").expect("`in` is a keyword");
        assert_eq!(kind, SyntaxKind::KW_IN);
        assert_eq!(kind.keyword_text(), Some("in"));
        assert!(
            kind.is_keyword(),
            "every kind produced by from_keyword must satisfy is_keyword"
        );
    }

    #[test]
    fn non_keywords_do_not_classify_as_keywords() {
        assert_eq!(SyntaxKind::from_keyword("out"), None); // builtin, not keyword
        assert_eq!(SyntaxKind::from_keyword("x"), None);
        assert_eq!(SyntaxKind::from_keyword("Int"), None); // type name is an ident
        assert!(!SyntaxKind::Ident.is_keyword());
        assert!(!SyntaxKind::PLUS.is_keyword());
    }
}
