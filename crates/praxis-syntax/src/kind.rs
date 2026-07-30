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

#![allow(dead_code)] // node kinds fill in across Slice 4; keep them all now.

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
    /// A double-quoted text literal, e.g. `"hello"`.
    TextLit,
    /// A backtick-delimited parser template, e.g. `` `{x:int}` ``. The whole
    /// template is one token in Milestone 1; its interior is re-scanned by the
    /// input-parser lexer in Milestone 6 (§7).
    BacktickTemplate,

    // ---- Keywords (§4). All are lexed; only a subset is parsed in M1. ----
    KW_LET,      // `let`
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
    /// `&&` — logical and (REP-07). The lexer's max-munch keeps it one token, as
    /// it does `||`, so a bare `AMP` is never part of one.
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

    // ---- Tree nodes (produced by the parser; added incrementally in Slice 4) ----
    /// The root node of a parsed file.
    SOURCE_FILE,
    /// A `let name = expr` binding.
    LET_STMT,
    /// A `var name = expr` binding.
    VAR_STMT,
    /// A bare expression used as a statement.
    EXPR_STMT,
    /// A reassignment statement: `name = expr` or `name += expr` etc. (§4.2).
    ASSIGN_STMT,
    /// A top-level or nested `fn` declaration.
    FN_ITEM,
    /// A `struct Name { field: Type, … }` declaration (M7, §4.5).
    STRUCT_ITEM,
    /// An `enum Name { Variant, Variant(Type), … }` declaration (M7, §4.6).
    ENUM_ITEM,
    /// One variant of an enum: `Name` or `Name(Type, …)`.
    ENUM_VARIANT,
    /// The `{ field: Type, … }` body of a struct declaration.
    FIELD_LIST,
    /// A single `name: Type` field of a struct.
    FIELD,
    /// A `Name { field: expr, … }` record-literal expression (M7, §4.5).
    RECORD_LIT_EXPR,
    /// A `receiver.0` tuple-element expression (REP-08, §4.4).
    ///
    /// Its own kind rather than a `FIELD_EXPR` holding an `IntLit`: an element is
    /// selected by **position** and the index must be a literal, where a field is
    /// selected by name — two different operations that lower to two different
    /// runtime calls.
    TUPLE_INDEX_EXPR,
    /// A `receiver.field` field-access expression (M7, §4.5).
    FIELD_EXPR,
    /// A `match scrutinee { pattern => expr, … }` expression (M7, §4.6/§4.11).
    MATCH_EXPR,
    /// A closure expression `|params| expr` (M7, §4.10). Bare `PIPE` claims the
    /// `|` (lexer max-munch keeps `||` as logical-or `PIPE2`).
    CLOSURE_EXPR,
    /// One `pattern => expr` arm of a match expression.
    MATCH_ARM,
    /// A pattern (M7, §4.6): wildcard `_`, literal, variable bind, enum variant,
    /// or tuple/record destructuring.
    PATTERN,
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
    /// A `for pat in iter { ... }` expression (M8, §4.11).
    FOR_EXPR,
    /// A `loop { ... }` expression (M8, §4.11).
    LOOP_EXPR,
    /// A `break [expr]` expression (M8, §4.11).
    BREAK_EXPR,
    /// A `continue` expression (M8, §4.11).
    CONTINUE_EXPR,
    /// A `return [expr]` expression (M8, §4.11).
    RETURN_EXPR,
    /// A `callee(args)` call expression (covers `out(...)`).
    CALL_EXPR,
    /// A `receiver.method(args)` method-call expression (M5, §16.2).
    METHOD_CALL_EXPR,
    /// The `(arg, arg, ...)` argument list of a call.
    ARG_LIST,
    /// A path: an identifier or a dotted name.
    PATH_EXPR,
    /// A literal value (`IntLit`/`TextLit`/`true`/`false`/backtick template).
    LITERAL,
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
    /// A type written in source. M2 covers scalar names (`Int`, `Text`, …),
    /// tuple types, and function types; richer type syntax lands with the
    /// constructs that need it.
    TYPE_REF,
    /// A tuple type `(T, U, …)`. A parenthesized single type `(T)` is just `T`,
    /// so this always carries two or more elements.
    TUPLE_TYPE,
    /// A function type `(P0, P1, …) -> R`.
    FN_TYPE,
    /// A parse-error placeholder node wrapping tokens the parser could not
    /// place. Recovery (§15.2) emits these so the tree stays well-formed.
    PARSE_ERROR,
    // ---- Input-parser expression nodes (M6, §7) ----
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
    /// A named argument inside a parser constructor call (M9, §7.5):
    /// `name: parser_expr`, e.g. `rules: lines(int)` in heterogeneous
    /// `sections`, or `skip: whitespace` in `chars`. Holds the name ident, the
    /// `:`, and the parser-expr value.
    PARSER_NAMED_ARG,
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
    /// second list: the previous hand-written list omitted `KW_IN`, and the
    /// test that was supposed to catch that copied the same incomplete list.
    #[must_use]
    pub fn is_keyword(self) -> bool {
        self.keyword_text().is_some()
    }

    /// Whether this kind is one of the three shapes a written type annotation
    /// can take: a name (with or without bracketed arguments), a tuple, or a
    /// function type.
    ///
    /// The set lives here, once, because everything that looks at an annotation
    /// needs the same answer: `praxis_ast::TypeRef::cast` accepts exactly these
    /// kinds, and type resolution recurses through exactly these children. Each
    /// site used to spell the list out for itself, and the one that only
    /// listed `TYPE_REF` — the AST accessors — silently dropped every direct
    /// tuple and function annotation (TY-08).
    #[must_use]
    pub fn is_type_node(self) -> bool {
        matches!(self, Self::TYPE_REF | Self::TUPLE_TYPE | Self::FN_TYPE)
    }

    /// The largest discriminant. Sound because the enum declares no explicit
    /// discriminants, so its values are consecutive from zero — which
    /// [`SyntaxKind::from_raw_u16`] relies on and
    /// `every_raw_value_in_range_round_trips` checks.
    const LAST: u16 = SyntaxKind::PARSER_NAMED_ARG as u16;

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
            "let" => Self::KW_LET,
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
    /// inverse of [`from_keyword`]; handy for the formatter and diagnostics.
    #[must_use]
    pub fn keyword_text(self) -> Option<&'static str> {
        Some(match self {
            Self::KW_LET => "let",
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
        assert!(SyntaxKind::KW_LET.is_token());
        assert!(SyntaxKind::PLUS.is_token());
        assert!(SyntaxKind::EOF.is_token());
        assert!(!SyntaxKind::Whitespace.is_token()); // trivia, not a token
        assert!(!SyntaxKind::LET_STMT.is_token()); // node
        assert!(SyntaxKind::SOURCE_FILE.is_node());
        assert!(SyntaxKind::PARSE_ERROR.is_node());
        assert!(!SyntaxKind::Ident.is_node());
        assert!(!SyntaxKind::EOF.is_node());
    }

    #[test]
    fn keyword_round_trip() {
        // Every keyword round-trips through from_keyword/keyword_text.
        let all = [
            SyntaxKind::KW_LET,
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
