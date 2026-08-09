//! The Praxis parser.
//!
//! Recursive descent over statements and structure, with a Pratt (precedence
//! climbing) loop for arithmetic and the other binary operators (ADR-004). It
//! consumes the lexer's [`Token`] stream and emits a rowan green tree
//! (ADR-003) via [`GreenNodeBuilder`], retaining trivia so the tree is lossless
//! (§13.1). On an unexpected token it emits a `P0xx` diagnostic, wraps the stray
//! token in a [`SyntaxKind::PARSE_ERROR`] node, advances to a synchronization
//! point, and keeps going — that is the LSP-grade recovery required by §15.2.
//!
//! The grammar is §19: `fn`/`struct`/`enum` items, `var` bindings and
//! assignment, blocks, `if`/`while`/`for`/`loop`/`match`, and expressions —
//! literals, calls, closures, ranges, record and collection literals, patterns,
//! and `read`'s parser expressions (§7.1).

use praxis_source::diagnostic::sort_by_position;
use praxis_source::{BytePos, Span};
use praxis_source::{DiagCode, Diagnostic, FileId, FileSpan, Severity};
use praxis_syntax::{PraxisLanguage, SyntaxKind, SyntaxNode, Token};
use rowan::{GreenNodeBuilder, Language};

use crate::lex::{lex, LexOutput};

/// The result of parsing one source file: the lossless tree, the lexed tokens,
/// and any diagnostics (lex + parse merged).
#[derive(Debug)]
pub struct ParseOutput {
    /// The lossless syntax tree (trivia retained). Always produced, even when
    /// the input is malformed — error nodes ([`SyntaxKind::PARSE_ERROR`]) carry
    /// the tokens the parser could not place.
    pub tree: SyntaxNode,
    /// The token stream from the lex pass, kept for later LSP use (semantic
    /// tokens) and diagnostics.
    pub tokens: Vec<Token>,
    /// Lex (`T0xx`) and parse (`P0xx`) diagnostics, in source order.
    pub diagnostics: Vec<Diagnostic>,
}

/// Lex then parse `text` belonging to `file`.
///
/// This is the front-end entry point the CLI threads in after reading a file.
/// It never panics on malformed input; it always returns a tree and as many
/// diagnostics as it could gather.
pub fn parse(file: FileId, text: &str) -> ParseOutput {
    let LexOutput {
        tokens,
        mut diagnostics,
    } = lex(file, text);
    let mut parser = Parser::new(file, text, &tokens);
    parser.parse_source_file();
    let (green, parse_diags) = parser.finish();
    diagnostics.extend(parse_diags);
    sort_by_position(&mut diagnostics);
    let tree = SyntaxNode::new_root(green);
    ParseOutput {
        tree,
        tokens,
        diagnostics,
    }
}

// ---------------------------------------------------------------------------
// Precedence table (Pratt climbing).
// ---------------------------------------------------------------------------

/// Binary operator binding power. Higher binds tighter. Left-associative
/// operators parse by calling `expr(min_bp + 1)` on the right; right-associative
/// ones would call `expr(min_bp)` (none exist, but the table is shaped to allow
/// it).
#[derive(Clone, Copy)]
struct BindingPower {
    left: u8,
    right: u8,
}

/// The binding power of `op`, or `None` if it is not a binary operator.
fn infix_binding_power(op: SyntaxKind) -> Option<BindingPower> {
    Some(match op {
        // Logical or (lowest of all).
        SyntaxKind::PIPE2 => bp(1, 2),
        // Range (§4.11, ADR-059): below comparison, above nothing but `||`. So
        // `0..n - 1` is `0..(n - 1)` — the bound is an arithmetic expression,
        // which is how every range in the corpus is written.
        SyntaxKind::DOT2 | SyntaxKind::DOT2EQ => bp(3, 4),
        // Logical and: **below comparison and above `..`**. The two rules that
        // matter are that `&&` binds tighter than `||` (`a || b && c` is
        // `a || (b && c)`) and looser than comparison (`a == b && c == d` is
        // `(a == b) && (c == d)`), which is §3.3's own shape. Where `&&` sits
        // relative to `..` is arbitrary — a range of `Bool`s and a range bound
        // that is a `&&` are both nonsense.
        SyntaxKind::AMP2 => bp(5, 6),
        // Comparison (non-associative in spirit; we parse left-assoc).
        SyntaxKind::EQ2
        | SyntaxKind::NEQ
        | SyntaxKind::LT
        | SyntaxKind::GT
        | SyntaxKind::LTEQ
        | SyntaxKind::GTEQ => bp(7, 8),
        // Additive.
        SyntaxKind::PLUS | SyntaxKind::MINUS => bp(9, 10),
        // Multiplicative.
        SyntaxKind::STAR | SyntaxKind::SLASH | SyntaxKind::PERCENT => bp(11, 12),
        _ => return None,
    })
}

/// The node kind an infix operator builds. Every operator but `..`/`..=` builds
/// a [`SyntaxKind::BIN_EXPR`]; a range is its own node because it is not an
/// operator applied to two numbers but a collection built from two bounds.
fn infix_node_kind(op: SyntaxKind) -> SyntaxKind {
    match op {
        SyntaxKind::DOT2 | SyntaxKind::DOT2EQ => SyntaxKind::RANGE_EXPR,
        _ => SyntaxKind::BIN_EXPR,
    }
}

/// Prefix (unary) operator binding power.
fn prefix_binding_power(op: SyntaxKind) -> Option<u8> {
    match op {
        // Above every infix operator, so `!a && b` is `(!a) && b` — §3.3's own
        // `!diagonals && dx != 0` — and `-a * b` is `(-a) * b`.
        SyntaxKind::MINUS | SyntaxKind::BANG => Some(13),
        // `read` is a prefix expression (§7.1): `read parser_expression`. Its
        // body is a parser-expression, not an ordinary expression, so it gets
        // the highest binding power (binds tighter than arithmetic).
        SyntaxKind::KW_READ => Some(15),
        _ => None,
    }
}

/// The compiler-owned type constructors, the only names that take an explicit
/// type-argument list in expression position (§3.3's `Counter[(Int, Int)]()`).
///
/// The parser has to know these **by name**, because nothing else can tell
/// `Counter[(Int, Int)]()` from `m[key]`: the brackets are the same and the
/// contents are ambiguous too — `Int` is a legal expression, and `(Int, Int)` a
/// legal tuple of two. `parse`'s own special case (§7.1) is the precedent for a
/// name-driven decision here.
///
/// The list is the §6.1 collection set plus `Option`, and `praxis-hir`'s
/// `is_type_ctor_name` is the other copy of it —
/// `the_parsers_type_constructors_are_the_compilers` asserts the two agree, so a
/// name in only one cannot go unnoticed.
pub const TYPE_CONSTRUCTOR_NAMES: &[&str] = &[
    "Vec", "Deque", "Map", "Set", "Counter", "MinHeap", "MaxHeap", "BitSet", "Grid", "Range",
    "Option",
];

/// Whether `op` is an assignment operator (statement-level reassignment, §4.2).
/// These are not infix expression operators; they are parsed as statements.
fn is_assignment_op(op: SyntaxKind) -> bool {
    matches!(
        op,
        SyntaxKind::EQ
            | SyntaxKind::PLUS_EQ
            | SyntaxKind::MINUS_EQ
            | SyntaxKind::STAR_EQ
            | SyntaxKind::SLASH_EQ
            | SyntaxKind::PERCENT_EQ
    )
}

/// Whether `kind` can start a match pattern (§4.6). Used to decide whether
/// to continue parsing arms after a newline (arms are comma-OR-newline separated).
///
/// This set must stay the set [`Parser::parse_pattern`] consumes: a pattern this
/// admits is one that parses, and one it rejects silently ends the arm list.
fn is_pattern_start(kind: SyntaxKind) -> bool {
    // The literal half is `SyntaxKind::is_pattern_literal` — the same set
    // `parse_pattern` consumes and `praxis_ast::Pattern` reads back.
    kind.is_pattern_literal()
        || matches!(
            kind,
            SyntaxKind::UNDERSCORE
                | SyntaxKind::Ident
                // A tuple pattern.
                | SyntaxKind::L_PAREN
                // A headless record pattern (ADR-091).
                | SyntaxKind::L_BRACE
        )
}

const fn bp(left: u8, right: u8) -> BindingPower {
    BindingPower { left, right }
}

// ---------------------------------------------------------------------------
// Statement separation (D8, ADR-049).
// ---------------------------------------------------------------------------

/// Whether a bare `Name { … }` in expression position is a record literal.
///
/// `if p { … }` is genuinely ambiguous: `p { … }` could be a record literal, or
/// `p` could be the condition and `{ … }` the then-block. The four keyword heads
/// — `if`, `while`, `for`'s iterator, `match`'s scrutinee — resolve it by
/// suppressing the literal, and the suppression follows the operands of the
/// expression they own.
///
/// It stops at the first bracket. Inside `(…)`, `[…]`, an argument list or a
/// block, the `{` cannot be the body the keyword is looking for, so there is
/// nothing left to disambiguate — which is why suppression is a parameter
/// threaded through the expression grammar rather than parser-wide state. State
/// would leak into every parenthesized subexpression and every match-arm body,
/// making valid record literals unwritable there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StructLit {
    Allowed,
    Suppressed,
}

/// What ended a statement.
///
/// Returning this rather than a `bool` is the point: a statement loop cannot
/// advance without either producing a separator value or emitting a diagnostic,
/// so "two statements adjacent with no separator" has no accepted
/// representation (D8, ADR-049).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StmtSeparator {
    /// An explicit `;`, consumed.
    Semicolon,
    /// A line break before the next token. The newline itself is trivia and
    /// stays trivia — the fact that it existed rides on the token after it.
    Newline,
    /// `}` or end of file: there is no next statement to separate from.
    EndOfBlock,
}

/// What `(1,)` and `(Int,)` are told, in one place because they are one rule
/// (§4.4). The expression and the type position both reach it through
/// [`Parser::parse_parenthesized`]; a second copy of the sentence is a second
/// thing to forget to change.
const ONE_ELEMENT_TUPLE_MSG: &str = "a tuple has two elements or more, so this comma names nothing";

// ---------------------------------------------------------------------------
// Parser state.
// ---------------------------------------------------------------------------

struct Parser<'t> {
    file: FileId,
    /// Full source text; needed to extract each token's spelling for the green
    /// tree (the lexer only stored spans, not text).
    text: &'t str,
    tokens: &'t [Token],
    /// Index into `tokens`; advances monotonically.
    cursor: usize,
    builder: GreenNodeBuilder<'static>,
    diagnostics: Vec<Diagnostic>,
}

impl<'t> Parser<'t> {
    fn new(file: FileId, text: &'t str, tokens: &'t [Token]) -> Parser<'t> {
        Parser {
            file,
            text,
            tokens,
            cursor: 0,
            builder: GreenNodeBuilder::new(),
            diagnostics: Vec::new(),
        }
    }

    // --- entry point ---

    fn parse_source_file(&mut self) {
        self.start_root_node(SyntaxKind::SOURCE_FILE);
        while !self.at_end() {
            // Emit any trivia leading each statement into the root node.
            self.eat_trivia();
            if self.at_end() {
                break;
            }
            let before = self.meaningful_index();
            if self.parse_stmt() {
                // A statement is separated from the next by `;`, a newline, or
                // the end of the file — and by nothing else.
                self.expect_stmt_separator();
            } else {
                // Recovery: skip to the next statement boundary.
                self.recover_to_stmt_boundary();
            }
            // Guarantee termination on any input (defense against infinite loops).
            self.ensure_progress(before);
        }
        // Trailing trivia belongs to the root.
        self.eat_trivia();
        self.finish_node();
    }

    // --- token cursor (over the meaningful stream; trivia emitted on sight) ---

    /// The kind of the current meaningful token (trivia skipped for the
    /// decision, but not consumed — it is emitted when bumped).
    fn peek(&mut self) -> SyntaxKind {
        self.nth_kind(0)
    }

    /// Kind of the meaningful token `n` positions ahead (0 = current).
    fn nth_kind(&mut self, n: usize) -> SyntaxKind {
        let mut idx = self.cursor;
        let mut want = n;
        while idx < self.tokens.len() {
            let kind = self.tokens[idx].kind;
            if kind.is_trivia() {
                idx += 1;
                continue;
            }
            if want == 0 {
                return kind;
            }
            want -= 1;
            idx += 1;
        }
        SyntaxKind::EOF
    }

    /// Whether the cursor is past the last meaningful token.
    fn at_end(&mut self) -> bool {
        self.peek() == SyntaxKind::EOF
    }

    /// True iff a line break sits between the previous meaningful token and the
    /// current one (D8, ADR-049).
    ///
    /// The flag lives on the token *after* the break, so this answer does not
    /// depend on whether the intervening trivia has already been emitted into
    /// the tree.
    fn newline_before(&self) -> bool {
        self.newline_before_nth(0)
    }

    /// [`Parser::newline_before`] about the meaningful token `n` positions ahead
    /// (0 = current). The field-vs-method decision looks one token past the name,
    /// and `p.x\n(a, b)` must not be a method call for the same reason
    /// `10\n(a, b)` must not be one.
    fn newline_before_nth(&self, n: usize) -> bool {
        self.tokens[self.cursor..]
            .iter()
            .filter(|token| !token.kind.is_trivia())
            .nth(n)
            .is_some_and(|token| token.preceded_by_newline)
    }

    /// True iff the `(` here opens an **argument list** for the expression that
    /// precedes it, rather than beginning something new.
    ///
    /// A `(` begins three things: a parenthesized expression, a tuple, and a
    /// **tuple pattern**, in a match arm and in a `for` binding. So a line-leading
    /// `(` is ambiguous in exactly the way ADR-049's rule is written to settle:
    ///
    /// ```text
    /// match p {
    ///     (0, 0) => 10
    ///     (a, b) => a + b     // a second arm, not `10(a, b)`
    /// }
    /// ```
    ///
    /// A match arm has no workaround for the ambiguity — a tuple pattern *is* how
    /// the arm is written — so the tie is broken by D8's own rule: a newline ends
    /// a statement. It is not consulted anywhere in the Pratt operator loop, so
    /// `1 +\n2` and a `.method()` chain across lines are unaffected, and a `(`
    /// that opens an expression is unaffected — only a `(` asked to *continue* one
    /// is.
    ///
    /// The cost is stated rather than hidden: a call whose callee ends a line and
    /// whose argument list begins the next (`f\n(1)`) is two expressions, and the
    /// fix is to move the `(` up.
    fn at_argument_list(&mut self) -> bool {
        self.at(SyntaxKind::L_PAREN) && !self.newline_before()
    }

    /// True iff the `[` here opens a **subscript** on the expression that
    /// precedes it, rather than beginning a list literal.
    ///
    /// [`at_argument_list`](Self::at_argument_list)'s rule at the second bracket.
    /// A `[` both begins a list literal and continues the expression before it,
    /// and the two spellings are the same two characters:
    ///
    /// ```text
    /// var n = total
    /// [1, 2, 3]           // a list literal, not `total[1, 2, 3]`
    /// ```
    ///
    /// So the tie is broken by position, exactly as it is for `(`: a `[` on the
    /// same line as what precedes it subscripts that expression, and a
    /// line-leading `[` starts a new one. `m[k]`, `grid[x, y]` and `m[k][j]` are
    /// unaffected — every one of them is written on one line — and the cost is
    /// the mirror of the argument-list rule's: a subscript whose receiver ends a
    /// line and whose bracket begins the next is two expressions, and the fix is
    /// to move the `[` up.
    fn at_subscript(&mut self) -> bool {
        self.at(SyntaxKind::L_BRACK) && !self.newline_before()
    }

    /// True iff a value follows `break`/`return` on the same line.
    ///
    /// Two questions, both of which must say yes: the next token has to be able
    /// to begin an expression at all (a `;`, `}`, `)`, `,`, `else` or `in`
    /// cannot), and it has to be on *this* line — `return\n1` is a value-less
    /// return followed by a separate statement, which is the second half of
    /// D8's rule.
    fn starts_expr(&mut self) -> bool {
        if self.newline_before() {
            return false;
        }
        // Consume trivia so the cursor lands on the meaningful token, then check
        // whether that token can begin an expression.
        self.eat_trivia();
        let k = self
            .tokens
            .get(self.cursor)
            .map(|t| t.kind)
            .unwrap_or(SyntaxKind::EOF);
        use SyntaxKind::*;
        !matches!(
            k,
            EOF | SEMICOLON | R_BRACE | R_PAREN | COMMA | KW_ELSE | KW_IN
        )
    }

    /// Demand the separator that must follow a statement, and report which one
    /// it was — `;`, a line break, or the end of the enclosing block/file.
    ///
    /// `None` means there was none: two statements ran together on one line, and
    /// a `P002` has been emitted at the second one. The caller keeps parsing
    /// (one diagnostic per run-on, not a cascade), because the statement itself
    /// was well-formed and the next one usually is too.
    fn expect_stmt_separator(&mut self) -> Option<StmtSeparator> {
        if self.eat(SyntaxKind::SEMICOLON) {
            return Some(StmtSeparator::Semicolon);
        }
        if self.at_end() || self.at(SyntaxKind::R_BRACE) {
            return Some(StmtSeparator::EndOfBlock);
        }
        if self.newline_before() {
            return Some(StmtSeparator::Newline);
        }
        let span = self.current_span();
        self.error_with(
            DiagCode::ExpectedStatementSeparator,
            span,
            "expected `;` or a line break between statements",
        );
        None
    }

    /// The source text of the current meaningful token (trivia skipped). Used to
    /// special-case keywords-that-look-like-idents such as `parse` and parser
    /// constructor names (`lines`, `csv`, …).
    fn peek_text(&mut self) -> Option<&'t str> {
        let mut idx = self.cursor;
        while idx < self.tokens.len() {
            let kind = self.tokens[idx].kind;
            if kind.is_trivia() {
                idx += 1;
                continue;
            }
            let span = self.tokens[idx].span;
            return Some(&self.text[span.start().to_usize()..span.end().to_usize()]);
        }
        None
    }

    /// `true` if an updating store's operator starts here: an `Ident` spelling
    /// `min` or `max`, **immediately** followed by `=` (§6.2).
    ///
    /// Adjacency is the rule, exactly as it is for `+=`: `min=` is one operator
    /// spelled in two tokens because `min` is an identifier, and `min = x` with a
    /// space is two tokens that mean what they say. Checking the raw token stream
    /// rather than [`Parser::nth_kind`] is what makes that askable — `nth_kind`
    /// skips trivia, which is precisely the difference.
    ///
    /// `==` cannot be mistaken for it: the lexer's max-munch makes that one
    /// `EQ2` token.
    fn at_update_op(&mut self) -> bool {
        if !self.at(SyntaxKind::Ident) {
            return false;
        }
        if !matches!(self.peek_text(), Some("min" | "max")) {
            return false;
        }
        let ident = self.meaningful_index();
        self.tokens
            .get(ident + 1)
            .is_some_and(|t| t.kind == SyntaxKind::EQ)
    }

    /// `true` if a statement's `:bp` breakpoint marker starts here: a `:`
    /// **immediately** followed by an `Ident` spelling `bp` (§9.8).
    ///
    /// [`Parser::at_update_op`]'s shape, and adjacency is the rule for its
    /// reason: `bp` is an identifier, so `:bp` is one marker spelled in two
    /// tokens, and `: bp` with a space is a `:` followed by a name. The raw
    /// token stream is what makes that askable — [`Parser::nth_kind`] skips
    /// trivia, which is exactly the difference between the two spellings.
    ///
    /// A type annotation cannot be mistaken for it, because this is only ever
    /// asked at the *end* of a statement: `var x: Int = 1` has consumed its `:`
    /// inside [`Parser::parse_var`] long before this runs, and a `:` that
    /// survives to a statement's end begins nothing else in the grammar.
    fn at_breakpoint_marker(&mut self) -> bool {
        if !self.at(SyntaxKind::COLON) {
            return false;
        }
        let colon = self.meaningful_index();
        self.tokens.get(colon + 1).is_some_and(|t| {
            t.kind == SyntaxKind::Ident
                && &self.text[t.span.start().to_usize()..t.span.end().to_usize()] == "bp"
        })
    }

    /// Consume a trailing `:bp` marker into a [`SyntaxKind::BREAKPOINT`] node,
    /// if one is here. Answers whether it consumed anything.
    ///
    /// Called from each statement parser just before it closes its own node, so
    /// the marker is a *child of the statement it marks* rather than a sibling
    /// of it. That is what lets HIR lowering ask a statement node whether it
    /// carries one without re-deriving the association from source positions.
    fn eat_breakpoint_marker(&mut self) -> bool {
        if !self.at_breakpoint_marker() {
            return false;
        }
        // `start_node` sweeps the leading trivia into the *enclosing* node, so
        // the whitespace before `:bp` stays outside the marker. The two tokens
        // are adjacent by `at_breakpoint_marker`'s check, so neither bump needs
        // a sweep of its own.
        self.start_node(SyntaxKind::BREAKPOINT);
        self.bump_meaningful(); // `:`
        self.bump_meaningful(); // `bp`
        self.finish_node();
        true
    }

    /// `true` if the current meaningful token is `kind`.
    fn at(&mut self, kind: SyntaxKind) -> bool {
        self.peek() == kind
    }

    /// Consume the current meaningful token if it is `kind`, returning `true`.
    /// Emits any trivia encountered first.
    fn eat(&mut self, kind: SyntaxKind) -> bool {
        if self.at(kind) {
            self.bump();
            true
        } else {
            false
        }
    }

    /// Consume the current meaningful token and append it (plus any leading
    /// trivia) to the tree. Panics only if called at EOF — the grammar always
    /// checks `at`/`at_end` first, so this is unreachable in well-formed calls.
    fn bump(&mut self) {
        // Emit any trivia sitting before the token we are about to take.
        self.eat_trivia();
        self.bump_meaningful();
    }

    /// Consume the current token assuming trivia has already been emitted (no
    /// trivia sweep). Used after an explicit `eat_trivia` to keep trivia out of
    /// a node that is about to be opened.
    fn bump_meaningful(&mut self) {
        debug_assert!(self.cursor < self.tokens.len(), "bump past EOF");
        let token = self.tokens[self.cursor];
        if token.kind == SyntaxKind::EOF {
            // Do not advance past EOF; callers should not bump it.
            return;
        }
        self.emit_token(token);
        self.cursor += 1;
    }

    /// The index of the current *meaningful* token (skipping trivia). Used to
    /// detect whether a sub-parse made progress; an infinite loop anywhere in
    /// the grammar would show up as `meaningful_index()` not advancing.
    fn meaningful_index(&mut self) -> usize {
        let mut idx = self.cursor;
        while idx < self.tokens.len() && self.tokens[idx].kind.is_trivia() {
            idx += 1;
        }
        idx
    }

    /// Defense against catastrophic infinite loops: if the cursor did not
    /// advance past `before`, consume one token (in a PARSE_ERROR node) so the
    /// parser always makes progress. Every loop over statements/expressions
    /// calls this after each body iteration.
    ///
    /// This is a safety net, not the primary recovery mechanism — the grammar
    /// is written to always consume — but it guarantees termination on any
    /// input, which is a hard requirement (§19: "no panic on fuzzed input",
    /// and an OOM-kill from an unbounded loop is a panic in disguise).
    fn ensure_progress(&mut self, before: usize) {
        let now = self.meaningful_index();
        if now <= before && !self.at_end() {
            self.start_node(SyntaxKind::PARSE_ERROR);
            let span = self.current_span();
            self.error(span, "stuck: skipping token to make progress");
            self.bump();
            self.finish_node();
        }
    }

    /// Emit trivia (whitespace/comments) into the tree up to, but not
    /// including, the next meaningful token. Trivia is part of the lossless
    /// tree, so it must be appended even though it is ignored for decisions.
    fn eat_trivia(&mut self) {
        while self.cursor < self.tokens.len() {
            let kind = self.tokens[self.cursor].kind;
            if !kind.is_trivia() {
                break;
            }
            self.emit_token(self.tokens[self.cursor]);
            self.cursor += 1;
        }
    }

    /// Append a single token's text to the green tree.
    fn emit_token(&mut self, token: Token) {
        // Compute the byte range first so the immutable borrow of `self.text`
        // ends before the mutable borrow of `self.builder` begins.
        let start = token.span.start().to_u32() as usize;
        let end = token.span.end().to_u32() as usize;
        let text = &self.text[start..end];
        self.builder
            .token(PraxisLanguage::kind_to_raw(token.kind), text);
    }

    // --- green-tree helpers ---

    /// Open a node **on its first meaningful token** — the trivia in front of it
    /// is emitted first, so it lands in the enclosing node instead.
    ///
    /// This is the whole of the rule "a node never begins with trivia", and it
    /// belongs here rather than at every call site. It is what makes a node's
    /// span the node's own text: the `PATH_EXPR` for `a` in `var c = a + b`
    /// spans `"a"` and not `" a"`, so the caret in a diagnostic that underlines
    /// an expression starts at the expression.
    ///
    /// The root is the one node this cannot open ([`start_root_node`](Self::start_root_node)):
    /// there is nothing to emit trivia into before it exists.
    fn start_node(&mut self, kind: SyntaxKind) {
        self.eat_trivia();
        self.builder.start_node(PraxisLanguage::kind_to_raw(kind));
    }

    /// Open the root node. The only node opened *before* its leading trivia,
    /// because a token cannot be emitted with no node open — the root is where
    /// a file's leading trivia goes.
    fn start_root_node(&mut self, kind: SyntaxKind) {
        self.builder.start_node(PraxisLanguage::kind_to_raw(kind));
    }

    fn finish_node(&mut self) {
        self.builder.finish_node();
    }

    fn start_node_at(&mut self, cp: rowan::Checkpoint, kind: SyntaxKind) {
        self.builder
            .start_node_at(cp, PraxisLanguage::kind_to_raw(kind));
    }

    // --- diagnostics ---

    fn error(&mut self, span: Span, message: impl Into<String>) {
        self.error_with(DiagCode::UnexpectedToken, span, message);
    }

    fn error_with(&mut self, code: DiagCode, span: Span, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic::new(
            Severity::Error,
            code,
            message,
            FileSpan::new(self.file, span),
        ));
    }

    /// Consume `self` to produce the green node and the parse diagnostics.
    fn finish(self) -> (rowan::GreenNode, Vec<Diagnostic>) {
        let green = self.builder.finish();
        (green, self.diagnostics)
    }

    // -----------------------------------------------------------------------
    // Grammar.
    // -----------------------------------------------------------------------

    /// Parse one statement. Returns `false` if recovery was needed.
    fn parse_stmt(&mut self) -> bool {
        match self.peek() {
            SyntaxKind::KW_VAR => self.parse_var(),
            SyntaxKind::KW_FN => self.parse_fn_item(),
            SyntaxKind::KW_STRUCT => self.parse_struct_item(),
            SyntaxKind::KW_ENUM => self.parse_enum_item(),
            // `name = expr` / `name += expr` reassignment (§4.2).
            SyntaxKind::Ident if is_assignment_op(self.nth_kind(1)) => self.parse_assign_stmt(),
            _ => self.parse_expr_stmt(),
        }
    }

    /// `var name [: Type] = expr` — the language's one binding form (§4.2).
    fn parse_var(&mut self) -> bool {
        self.start_node(SyntaxKind::VAR_STMT);
        self.bump(); // `var`
        self.expect_binder("binding name");
        // Optional type annotation `: Type`.
        if self.eat(SyntaxKind::COLON) {
            self.parse_type();
        }
        self.expect(SyntaxKind::EQ, "`=`");
        self.parse_expr();
        self.eat_breakpoint_marker();
        self.finish_node();
        true
    }

    /// `name = expr` or `name += expr` (etc.) — reassignment to an existing
    /// binding (§4.2). The lhs is a bare name; a field or subscript target
    /// builds a `PLACE_ASSIGN_STMT` in
    /// [`parse_expr_stmt`](Self::parse_expr_stmt) instead.
    fn parse_assign_stmt(&mut self) -> bool {
        self.start_node(SyntaxKind::ASSIGN_STMT);
        self.bump(); // name
        self.bump(); // assignment operator (=, +=, ...)
        self.parse_expr();
        self.eat_breakpoint_marker();
        self.finish_node();
        true
    }

    /// `fn name(params) -> Ret { body }` (params and return type optional).
    fn parse_fn_item(&mut self) -> bool {
        self.start_node(SyntaxKind::FN_ITEM);
        self.bump(); // `fn`
        self.expect(SyntaxKind::Ident, "function name");
        if self.eat(SyntaxKind::L_PAREN) {
            self.start_node(SyntaxKind::PARAM_LIST);
            // Zero or more `name: Type` params separated by commas.
            if !self.at(SyntaxKind::R_PAREN) {
                loop {
                    let before = self.meaningful_index();
                    self.start_node(SyntaxKind::PARAM);
                    self.expect_binder("parameter name");
                    // The `: Type` annotation is OPTIONAL (§4.9, criterion 1):
                    // `fn manhattan(a, b) { … }` infers param types from use.
                    if self.eat(SyntaxKind::COLON) {
                        self.parse_type();
                    }
                    self.finish_node();
                    if !self.eat(SyntaxKind::COMMA) {
                        break;
                    }
                    // A trailing comma closes the list.
                    if self.at(SyntaxKind::R_PAREN) {
                        break;
                    }
                    // Guarantee termination on any input.
                    self.ensure_progress(before);
                }
            }
            self.expect(SyntaxKind::R_PAREN, "`)`");
            self.finish_node();
        }
        // Optional `-> Type` return annotation.
        if self.eat(SyntaxKind::THIN_ARROW) {
            self.parse_type();
        }
        // Body.
        if self.at(SyntaxKind::L_BRACE) {
            self.parse_block();
        } else {
            let span = self.current_span();
            self.error(span, "expected `{` to begin function body");
        }
        self.finish_node();
        true
    }

    /// `struct Name { field: Type, … }` (§4.5). The field list is a
    /// `FIELD_LIST` of `FIELD` children, each `name: Type`.
    fn parse_struct_item(&mut self) -> bool {
        self.start_node(SyntaxKind::STRUCT_ITEM);
        self.bump(); // `struct`
        self.expect(SyntaxKind::Ident, "struct name");
        self.expect(SyntaxKind::L_BRACE, "`{` to begin struct fields");
        self.start_node(SyntaxKind::FIELD_LIST);
        if !self.at(SyntaxKind::R_BRACE) {
            loop {
                let before = self.meaningful_index();
                self.start_node(SyntaxKind::FIELD);
                self.expect(SyntaxKind::Ident, "field name");
                self.expect(SyntaxKind::COLON, "`:` before field type");
                self.parse_type();
                self.finish_node();
                // A comma **or** a line break separates fields — §4.5's own
                // `struct Point { x: Int\n y: Int }` writes the second, and a
                // trailing comma closes the list either way.
                if !self.member_separator("struct fields") {
                    break;
                }
                self.ensure_progress(before);
            }
        }
        self.expect(SyntaxKind::R_BRACE, "`}` to end struct fields");
        self.finish_node(); // FIELD_LIST
        self.finish_node(); // STRUCT_ITEM
        true
    }

    /// Whether another member of a brace-delimited declaration follows, having
    /// consumed the separator between them.
    ///
    /// A member is followed by a comma **or** a line break, which is the rule
    /// match arms use (D8, ADR-049) and the one §4.5's and §4.6's own
    /// declarations are written with:
    ///
    /// ```praxis
    /// struct Point {
    ///     x: Int
    ///     y: Int
    /// }
    /// ```
    ///
    /// The two separators are interchangeable and a trailing comma still closes
    /// the list, so the answer is `false` at the closing brace whichever one
    /// preceded it. A member that follows with *neither* is reported at the
    /// same code a run-together statement is — and then parsed anyway, because
    /// the mistake is the separator and not the member.
    fn member_separator(&mut self, what: &str) -> bool {
        let comma = self.eat(SyntaxKind::COMMA);
        // A closing brace ends the list, and an `Ident` is the only token that
        // can begin either kind of member — anything else is a mistake the
        // member's own parser will report.
        if self.at(SyntaxKind::R_BRACE) || !self.at(SyntaxKind::Ident) {
            return false;
        }
        if !comma && !self.newline_before() {
            let span = self.current_span();
            self.error_with(
                DiagCode::ExpectedStatementSeparator,
                span,
                format!("expected `,` or a line break between {what}"),
            );
        }
        true
    }

    /// `enum Name { Variant, Variant(Type, …), … }` (§4.6). Each variant is
    /// an `ENUM_VARIANT` node: a name optionally followed by `( type_list )`.
    fn parse_enum_item(&mut self) -> bool {
        self.start_node(SyntaxKind::ENUM_ITEM);
        self.bump(); // `enum`
        self.expect(SyntaxKind::Ident, "enum name");
        self.expect(SyntaxKind::L_BRACE, "`{` to begin enum variants");
        if !self.at(SyntaxKind::R_BRACE) {
            loop {
                let before = self.meaningful_index();
                self.start_node(SyntaxKind::ENUM_VARIANT);
                self.expect(SyntaxKind::Ident, "variant name");
                // Optional payload: `( Type, Type, … )`.
                if self.eat(SyntaxKind::L_PAREN) {
                    if !self.at(SyntaxKind::R_PAREN) {
                        loop {
                            let pbefore = self.meaningful_index();
                            self.parse_type();
                            if !self.eat(SyntaxKind::COMMA) {
                                break;
                            }
                            // A trailing comma closes the list.
                            if self.at(SyntaxKind::R_PAREN) {
                                break;
                            }
                            self.ensure_progress(pbefore);
                        }
                    }
                    self.expect(SyntaxKind::R_PAREN, "`)` to close variant payload");
                }
                self.finish_node(); // ENUM_VARIANT
                                    // A comma **or** a line break, as §4.6's
                                    // own `enum Tile { Empty\n Wall\n … }`
                                    // writes it.
                if !self.member_separator("enum variants") {
                    break;
                }
                self.ensure_progress(before);
            }
        }
        self.expect(SyntaxKind::R_BRACE, "`}` to end enum variants");
        self.finish_node(); // ENUM_ITEM
        true
    }

    /// A bare expression used as a statement.
    fn parse_expr_stmt(&mut self) -> bool {
        if self.at(SyntaxKind::L_BRACE) {
            // A block as a statement is parsed as an expression.
            self.start_node(SyntaxKind::EXPR_STMT);
            self.parse_expr();
            self.eat_breakpoint_marker();
            self.finish_node();
            return true;
        }
        // The expression is parsed before the statement node is opened, because
        // what it turns out to be decides the kind: an assignment operator after
        // it makes the whole thing a `PLACE_ASSIGN_STMT` whose first child is the
        // target (`counts[point] += 1`), and anything else an `EXPR_STMT`.
        //
        // A bare `name` target never reaches here — `parse_stmt` sends
        // `name = …` to `parse_assign_stmt` on the token after the name — so the
        // targets this sees are the compound ones. Which of them is a place is
        // inference's answer rather than the parser's: `p.x = 1` is a field store
        // (§4.5) and `f() = 1` is a well-formed *shape* whose mistake is that it
        // names no storage (`Y021`), and a parse error there says only "expected
        // a statement separator" about either.
        self.eat_trivia();
        let cp = self.checkpoint_lhs();
        self.parse_expr();
        if is_assignment_op(self.peek()) {
            self.start_node_at(cp, SyntaxKind::PLACE_ASSIGN_STMT);
            self.bump(); // assignment operator
            self.parse_expr();
            self.eat_breakpoint_marker();
            self.finish_node(); // PLACE_ASSIGN_STMT
            return true;
        }
        // `distance[key] min= candidate` (§6.2). The operator is two tokens, so
        // the parser decides it here rather than the lexer: `min` is an
        // identifier everywhere else, and a lexer rule would take it away from
        // every program that names the prelude helper.
        if self.at_update_op() {
            self.start_node_at(cp, SyntaxKind::PLACE_ASSIGN_STMT);
            self.start_node(SyntaxKind::UPDATE_OP);
            self.bump(); // `min` / `max`
            self.bump(); // `=`
            self.finish_node(); // UPDATE_OP
            self.parse_expr();
            self.eat_breakpoint_marker();
            self.finish_node(); // PLACE_ASSIGN_STMT
            return true;
        }
        self.start_node_at(cp, SyntaxKind::EXPR_STMT);
        self.eat_breakpoint_marker();
        self.finish_node();
        true
    }

    /// `{ stmt; stmt; expr }` — a block. The last expression is the block's
    /// value (§4.11). Every item is either a statement or a trailing expression.
    fn parse_block(&mut self) {
        self.start_node(SyntaxKind::BLOCK_EXPR);
        self.bump(); // `{`
        while !self.at(SyntaxKind::R_BRACE) && !self.at_end() {
            self.eat_trivia();
            if self.at(SyntaxKind::R_BRACE) || self.at_end() {
                break;
            }
            let before = self.meaningful_index();
            // Every item is a statement (var/fn/assignment) or a bare
            // expression statement (which may be the trailing expression). The
            // dispatch in parse_stmt covers all of these.
            self.parse_stmt();
            // A `;` is optional only because a newline separates just as well;
            // one of the two (or the closing `}`) has to be there.
            self.expect_stmt_separator();
            // Guarantee termination on any input.
            self.ensure_progress(before);
        }
        self.expect(SyntaxKind::R_BRACE, "`}` to close block");
        self.finish_node();
    }

    // --- expressions (Pratt climbing) ---

    /// Entry into expression parsing at the lowest binding power.
    ///
    /// This is the *bracketed* entry: a record literal is legal here. Every
    /// caller that is inside `(…)`, an argument list, a block or a record body
    /// uses it. The four keyword heads use [`Parser::parse_expr_no_struct_lit`].
    fn parse_expr(&mut self) {
        self.parse_expr_bp(0, StructLit::Allowed);
    }

    /// Entry into an expression that a `{` will terminate: an `if`/`while`
    /// condition, a `for` iterator, a `match` scrutinee.
    fn parse_expr_no_struct_lit(&mut self) {
        self.parse_expr_bp(0, StructLit::Suppressed);
    }

    fn parse_expr_bp(&mut self, min_bp: u8, lit: StructLit) {
        // Capture the builder position *before* the left operand is emitted, so
        // a later infix operator can wrap (lhs op rhs) into a BIN_EXPR via
        // start_node_at. This is the standard rowan Pratt idiom.
        let cp = self.checkpoint_lhs();
        // Parse the left-hand side (prefix / atom).
        self.parse_prefix(lit);

        // Fold infix operators while their binding power is high enough.
        loop {
            let op = self.peek();
            if op == SyntaxKind::EOF {
                break;
            }
            let Some(bp) = infix_binding_power(op) else {
                break;
            };
            if bp.left < min_bp {
                break;
            }
            // Wrap the already-emitted lhs + operator + rhs in a BIN_EXPR (or a
            // RANGE_EXPR), retroactively opening the node at the checkpoint taken
            // before lhs.
            self.start_node_at(cp, infix_node_kind(op));
            self.bump(); // operator
                         // The operands of a suppressed expression are suppressed too:
                         // `if a == p { … }` has the same ambiguity `if p { … }` has.
            self.parse_expr_bp(bp.right, lit);
            self.finish_node();
        }
    }

    /// Prefix expression: unary operators, `read`, then an atom or a
    /// parenthesized expression. Followed by any postfix `expr(args)` calls
    /// (§4.10) — calling a closure retrieved from a collection
    /// (`fs.get(0)(100)`), the result of another call (`f(1)(2)`), a paren
    /// (`(|x| x*3)(14)`), etc.
    fn parse_prefix(&mut self, lit: StructLit) {
        // Capture the builder position before the primary is emitted, so a
        // postfix `expr(args)` can wrap the whole primary as the call's callee
        // via start_node_at (same idiom as the method-call loop).
        let cp = self.checkpoint_lhs();
        let op = self.peek();
        // `read parser_expression` (§7.1): a prefix expression whose body is a
        // parser-expression grammar, not an ordinary expression.
        if op == SyntaxKind::KW_READ {
            self.start_node(SyntaxKind::READ_EXPR);
            self.bump(); // `read`
            self.parse_parser_expr();
            self.finish_node();
        } else if op == SyntaxKind::PIPE || op == SyntaxKind::PIPE2 {
            // `|params| expr` closure (§4.10) — and `|| expr`, the
            // zero-parameter one (§4.2).
            //
            // The lexer's max-munch makes `||` one token (`PIPE2`), so the empty
            // parameter list and logical-or are spelled identically. The tie is
            // broken by **position**, the same rule `min=` and `[` use: this
            // function is only ever called where an expression must *begin*, and
            // a binary operator has no left operand there. So a `||` here is the
            // empty parameter list and nothing else, and a `||` between two
            // operands is still logical-or — the infix loop reads it, and it
            // never comes through here.
            self.parse_closure(lit);
        } else if let Some(bp) = prefix_binding_power(op) {
            self.start_node(SyntaxKind::UNARY_EXPR);
            self.bump(); // unary operator
            self.parse_expr_bp(bp, lit);
            self.finish_node();
        } else {
            self.parse_atom(lit);
        }
        self.parse_postfix(cp);
    }

    /// The postfix chain on the expression preceding the current position:
    /// `expr(args)`, `expr.method(args)` and `expr.field`, in **any order and
    /// any number of times**, left-associatively.
    ///
    /// One loop over all three forms rather than a call loop followed by a
    /// field/method loop: two sequential loops cannot express `(fs).get(0)(100)`
    /// (a call on the result of a method call), because control never returns
    /// from the second loop to the first.
    ///
    /// `cp` is the checkpoint taken *before* the primary was emitted, so
    /// `start_node_at(cp, …)` retroactively wraps it as the new node's first
    /// child. It is NOT updated between iterations: each link in `a.b().c()`
    /// must wrap the entire preceding expression, which starts at the original
    /// `cp`.
    fn parse_postfix(&mut self, cp: rowan::Checkpoint) {
        loop {
            match self.peek() {
                // An argument list, and **only on the same line**. See
                // [`Parser::at_argument_list`] for why.
                SyntaxKind::L_PAREN if self.at_argument_list() => {
                    self.start_node_at(cp, SyntaxKind::CALL_EXPR);
                    self.bump(); // `(`
                    self.parse_arg_list();
                    self.finish_node(); // CALL_EXPR
                }
                // `m[key]`, `grid[x, y]` — a subscript. A postfix form like the
                // other two, so `grid[x, y].len()` and `m[k][j]` chain without a
                // second loop.
                //
                // And **only on the same line**, for the reason the `(` above is:
                // a `[` also begins a list literal. See [`Parser::at_subscript`].
                SyntaxKind::L_BRACK if self.at_subscript() => {
                    self.start_node_at(cp, SyntaxKind::INDEX_EXPR);
                    self.bump(); // `[`
                                 // A subscript selects *something*, so unlike a call's
                                 // argument list an empty one is a syntax error rather than an
                                 // arity the catalog happens not to have a row for.
                    if self.at(SyntaxKind::R_BRACK) {
                        let span = self.current_span();
                        self.error(span, "expected an index expression");
                    }
                    self.parse_arg_list_until(SyntaxKind::R_BRACK, "`]`");
                    self.finish_node(); // INDEX_EXPR
                }
                SyntaxKind::DOT => {
                    self.bump(); // `.`
                                 // `p.0` — a tuple element, selected by position.
                                 // The lexer guarantees the literal is an integer here: a
                                 // digit run immediately after a `.` takes no fraction, so
                                 // `t.0.1` is two indices and not an index and a float.
                    if self.at(SyntaxKind::IntLit) {
                        self.start_node_at(cp, SyntaxKind::TUPLE_INDEX_EXPR);
                        self.bump(); // the index
                        self.finish_node(); // TUPLE_INDEX_EXPR
                        continue;
                    }
                    if !self.at(SyntaxKind::Ident) {
                        let span = self.current_span();
                        self.error(span, "expected a name or a tuple index after `.`");
                        break;
                    }
                    // Disambiguate field access (`p.x`) from method call
                    // (`p.x()`): an IDENT followed by `(` **on the same line** is
                    // a method call. The line break matters here for the same
                    // reason it does at the top of this loop — `p.x\n(a, b)` is a
                    // match arm body followed by a tuple pattern, not
                    // `p.x(a, b)`.
                    if self.nth_kind(1) == SyntaxKind::L_PAREN && !self.newline_before_nth(1) {
                        self.bump(); // method name
                        self.start_node_at(cp, SyntaxKind::METHOD_CALL_EXPR);
                        self.bump(); // `(`
                        self.parse_arg_list();
                        self.finish_node(); // METHOD_CALL_EXPR
                    } else {
                        self.start_node_at(cp, SyntaxKind::FIELD_EXPR);
                        self.bump(); // field name
                        self.finish_node(); // FIELD_EXPR
                    }
                }
                _ => break,
            }
        }
    }

    /// The `arg, arg, …)` of a call, with the opening `(` already consumed.
    /// Emits the `ARG_LIST` node and consumes the closing `)`.
    fn parse_arg_list(&mut self) {
        self.parse_arg_list_until(SyntaxKind::R_PAREN, "`)`");
    }

    /// The `arg, arg, …<closer>` of a call or a subscript, with the opener
    /// already consumed. Emits the `ARG_LIST` node and consumes `closer`.
    ///
    /// One function for both brackets: `grid[x, y]` (§6.4) is a comma-separated
    /// expression list with the same trailing-comma rule a call's has, and a
    /// second copy of a list loop is a second place for that rule to be missing.
    fn parse_arg_list_until(&mut self, closer: SyntaxKind, closer_msg: &str) {
        self.start_node(SyntaxKind::ARG_LIST);
        if !self.at(closer) {
            loop {
                let before = self.meaningful_index();
                self.parse_expr();
                if !self.eat(SyntaxKind::COMMA) {
                    break;
                }
                // A trailing comma closes the list rather than opening another
                // argument — §3.3's own `max(\n abs(dx),\n abs(dy),\n)` writes
                // one, and counting it would make the call's arity one too high.
                if self.at(closer) {
                    break;
                }
                // Guarantee termination on any input.
                self.ensure_progress(before);
            }
        }
        self.expect(closer, closer_msg);
        self.finish_node(); // ARG_LIST
    }

    /// The `[Type, …]` type-argument list of a constructor call, with the name
    /// already emitted and the `[` still current.
    ///
    /// A type-argument list exists only on a constructor *call*, so the `(` after
    /// it is required: `Counter[Int]` alone names a type in value position, which
    /// no expression grammar accepts.
    fn parse_type_arg_list(&mut self) {
        self.start_node(SyntaxKind::TYPE_ARG_LIST);
        self.bump(); // `[`
        if self.at(SyntaxKind::R_BRACK) {
            let span = self.current_span();
            self.error(span, "expected a type argument");
        } else {
            loop {
                let before = self.meaningful_index();
                self.parse_type();
                if !self.eat(SyntaxKind::COMMA) {
                    break;
                }
                // A trailing comma closes the list.
                if self.at(SyntaxKind::R_BRACK) {
                    break;
                }
                // Guarantee termination on any input.
                self.ensure_progress(before);
            }
        }
        self.expect(SyntaxKind::R_BRACK, "`]`");
        self.finish_node(); // TYPE_ARG_LIST
                            // The `(` is required and it is on **this** line,
                            // so the report and what `parse_name_or_call` goes
                            // on to build cannot disagree.
        if !self.at_argument_list() {
            let span = self.current_span();
            self.error(
                span,
                "`(` — a type argument list belongs to a constructor call",
            );
        }
    }

    /// `|params| expr` — a closure expression (§4.10). Each parameter is a
    /// **pattern**, optionally annotated `: Type`, separated by commas, between two
    /// `|`. The body is a single expression (which may be a `{ block }`). Closures
    /// capture outer variables automatically (§4.10); the capture analysis is in
    /// HIR.
    ///
    /// A parameter is a pattern for the reason a `for` binding is one:
    /// destructuring in binding position **is** a pattern, so `|(a, b)| a + b`
    /// needs no grammar of its own.
    ///
    /// Nothing here can be confused with the body: a parameter is followed by `,`,
    /// `:` or `|`, never by an expression, so a record pattern's brace has nothing
    /// else waiting for it.
    ///
    /// The body inherits the ambient suppression rather than resetting it: `|`
    /// is not a bracket the grammar can close over, so a closure written
    /// directly as an `if` condition has the same ambiguity a name does.
    fn parse_closure(&mut self, lit: StructLit) {
        self.start_node(SyntaxKind::CLOSURE_EXPR);
        // `|| expr` — the zero-parameter closure. One `PIPE2` token is *both*
        // pipes: there is no parameter list between them to parse and no closing
        // `|` to demand. The token stays whole rather than being split into two,
        // because the tree's job is to round-trip the source and `||` is what
        // the source says; the node kind is what carries the meaning.
        if self.eat(SyntaxKind::PIPE2) {
            self.parse_expr_bp(0, lit);
            self.finish_node();
            return;
        }
        self.bump(); // `|`
                     // Zero or more `pattern` or `pattern: Type` params separated by commas.
        if !self.at(SyntaxKind::PIPE) {
            loop {
                let before = self.meaningful_index();
                self.start_node(SyntaxKind::PARAM);
                self.parse_pattern();
                if self.eat(SyntaxKind::COLON) {
                    self.parse_type();
                }
                self.finish_node();
                if !self.eat(SyntaxKind::COMMA) {
                    break;
                }
                // A trailing comma closes the list.
                if self.at(SyntaxKind::PIPE) {
                    break;
                }
                self.ensure_progress(before);
            }
        }
        self.expect(SyntaxKind::PIPE, "`|` to close closure parameters");
        // Body: a single expression (which may be a `{ block }`).
        self.parse_expr_bp(0, lit);
        self.finish_node();
    }

    /// Smallest expression: literals (including interpolated ones), names,
    /// calls, parenthesized expressions, list literals, blocks, and the
    /// keyword-headed expressions (`if`, `while`, `for`, `loop`, `match`,
    /// `break`, `continue`, `return`).
    fn parse_atom(&mut self, lit: StructLit) {
        let kind = self.peek();
        match kind {
            // The `LITERAL` set is `SyntaxKind::is_literal_token`: this is the
            // site that *builds* the node and `praxis_ast::Literal::token` is the
            // one that reads it back, so both must ask the same predicate.
            // `true`/`false` are literals too — one arm covers every literal.
            k if k.is_literal_token() => {
                self.start_node(SyntaxKind::LITERAL);
                self.bump();
                self.finish_node();
            }
            SyntaxKind::InterpOpen => self.parse_interp(),
            SyntaxKind::L_PAREN => self.parse_paren(),
            SyntaxKind::L_BRACK => self.parse_list(),
            SyntaxKind::L_BRACE => self.parse_block(),
            SyntaxKind::KW_IF => self.parse_if(),
            SyntaxKind::KW_WHILE => self.parse_while(),
            SyntaxKind::KW_FOR => self.parse_for(),
            SyntaxKind::KW_LOOP => self.parse_loop(),
            SyntaxKind::KW_BREAK => self.parse_break(),
            SyntaxKind::KW_CONTINUE => self.parse_continue(),
            SyntaxKind::KW_RETURN => self.parse_return(),
            SyntaxKind::KW_MATCH => self.parse_match(),
            SyntaxKind::Ident => self.parse_name_or_call(lit),
            _ => {
                // Nothing recognizable. CRITICAL: we must make forward progress
                // here — emitting a diagnostic without advancing the cursor
                // would let every caller loop spin forever (and balloon memory).
                // So consume the offending token (wrapped in a PARSE_ERROR) even
                // at EOF, where bump() is a no-op, we bail without consuming.
                if self.at_end() {
                    let span = self.current_span();
                    self.error(span, "expected an expression");
                } else {
                    self.start_node(SyntaxKind::PARSE_ERROR);
                    let span = self.current_span();
                    self.error(span, "expected an expression");
                    self.bump(); // guaranteed progress
                    self.finish_node();
                }
            }
        }
    }

    /// An interpolated text literal: `"a{x}b{y}"` (§8.1, ADR-147).
    ///
    /// The lexer has already decided the shape — it emits an `InterpOpen`, then
    /// the hole's ordinary tokens, then an `InterpMiddle` for each further hole,
    /// then exactly one `InterpClose` — so this is a straight walk of that run
    /// rather than a scan of any kind. **Nothing here re-reads the source text**,
    /// which is the point: the hole's expression is parsed by
    /// [`parse_expr`](Self::parse_expr), so it is an ordinary subtree whose names
    /// are tokens at their own ranges, and closure capture analysis sees them
    /// (ADR-147 decision 1).
    ///
    /// The loop is bounded by the token stream, not by a count, and the
    /// `InterpClose` is `expect`ed rather than assumed: a token stream missing
    /// one cannot be produced by the lexer, but a parser that assumed it would
    /// spin at EOF instead of reporting.
    fn parse_interp(&mut self) {
        self.start_node(SyntaxKind::INTERP_EXPR);
        self.bump(); // `"…{`
        loop {
            self.refuse_doubled_brace();
            self.parse_expr(); // the hole
            if self.at(SyntaxKind::InterpMiddle) {
                self.bump(); // `}…{`
                continue;
            }
            break;
        }
        self.expect(SyntaxKind::InterpClose, "`}` to close the interpolation");
        self.finish_node();
    }

    /// A hole whose expression *opens with a brace* is `{{`, and `{{` is not an
    /// escape in this language — ADR-147 decision 4 chose `\{` instead, so that
    /// `"…"` and `'…'` keep one shared escape table.
    ///
    /// Without this the doubling rule other languages use does not fail, it
    /// **means something else**: `{` opens the hole, `{}` is an empty block, `}`
    /// closes the hole, so `"a{{}}b"` is a well-typed program printing `aUnitb`,
    /// and `"a{{x}}b"` reports `N001` about a name the author thought they had
    /// escaped. A reader arriving from Rust, C# or Python writes `{{` first and
    /// gets no sign they were wrong, so the mistaken escape is refused where it
    /// is written and the message names the spelling that works.
    ///
    /// It costs a block as a hole's whole expression, which nothing wants: a
    /// hole is a rendering site, and `"{ var t = f(); t }"` is a sentence no one
    /// writes inside a string. A record literal is unaffected — that opens with
    /// its type's name, not with a brace.
    fn refuse_doubled_brace(&mut self) {
        if !self.at(SyntaxKind::L_BRACE) {
            return;
        }
        let span = self.current_span();
        self.error(
            span,
            "`{{` is not an escape for a literal brace: write `\\{` for a `{`, \
             or a value to render between single braces",
        );
    }

    fn parse_paren(&mut self) {
        // Either `( expr )` (PAREN_EXPR) or `( e1, e2, … )` (TUPLE_EXPR) — the
        // decision, and everything that follows from it, is
        // `parse_parenthesized`'s.
        let cp = self.checkpoint_lhs();
        self.bump(); // `(`
        self.parse_parenthesized(
            cp,
            SyntaxKind::TUPLE_EXPR,
            SyntaxKind::PAREN_EXPR,
            Self::parse_expr,
        );
    }

    /// A parenthesized list, in either position: `( x )` / `( T )` groups,
    /// `( a, b, … )` / `( T, U, … )` is a tuple. The caller has taken `cp`
    /// *before* the `(` and consumed the `(`; `element` parses one element —
    /// `parse_expr` in expression position, `parse_type` in type position.
    ///
    /// Which node this is cannot be known until a comma turns up after the first
    /// element, so the shared prefix is emitted first and the right kind is then
    /// opened *retroactively* at `cp`. Both `(` and `)` end up inside the single
    /// resulting node (no double nesting).
    ///
    /// One function for both positions so the §4.4 arity rule below is written
    /// once.
    fn parse_parenthesized(
        &mut self,
        cp: rowan::Checkpoint,
        tuple_kind: SyntaxKind,
        single_kind: SyntaxKind,
        mut element: impl FnMut(&mut Self),
    ) {
        if self.at(SyntaxKind::R_PAREN) {
            // Empty `()`, `single_kind` in both positions but for two reasons.
            // As an expression it is a degenerate paren expr, and it is `Unit` —
            // the same type `()` names in an annotation, and the value `out(())`
            // prints. As a type it is recorded as a plain `TYPE_REF` and left
            // for type resolution to reject.
            self.expect(SyntaxKind::R_PAREN, "`)`");
            self.start_node_at(cp, single_kind);
            self.finish_node();
            return;
        }
        element(self); // first element
        let is_tuple = self.at(SyntaxKind::COMMA);
        let mut elements = 1usize;
        let mut trailing_comma = None;
        if is_tuple {
            // Collect the remaining elements.
            loop {
                let before = self.meaningful_index();
                let comma = self.current_span();
                if !self.eat(SyntaxKind::COMMA) {
                    break;
                }
                if self.at(SyntaxKind::R_PAREN) {
                    // Trailing comma: `(a, b, )` — stop without another element.
                    trailing_comma = Some(comma);
                    break;
                }
                element(self);
                elements += 1;
                // Guarantee termination on any input.
                self.ensure_progress(before);
            }
        }
        // **A tuple has two elements or more** (§4.4). `(1,)` is the one spelling
        // that reaches here with fewer, and the language has nothing for it to
        // mean: `TupleElems` refuses to represent a one-element tuple, so typing
        // and lowering would have to disagree about what the node is. `(Int,)` is
        // the type-position spelling of the same mistake — an annotation naming a
        // type the language does not have.
        //
        // So the comma is refused here, and the node recovers as the grouping
        // the author most likely meant. `(1, 2,)` is untouched — a trailing
        // comma is punctuation at every arity the type exists at.
        let one_element_tuple = is_tuple && elements < 2;
        if one_element_tuple {
            let at = trailing_comma.unwrap_or_else(|| self.current_span());
            self.error(at, ONE_ELEMENT_TUPLE_MSG);
        }
        let kind = if is_tuple && !one_element_tuple {
            tuple_kind
        } else {
            single_kind
        };
        self.expect(SyntaxKind::R_PAREN, "`)`");
        self.start_node_at(cp, kind);
        self.finish_node();
    }

    /// `[ e1, e2, … ]` — a `Vec` literal (§6.1). The opening `[` is current.
    ///
    /// One node kind for every arity, including the empty `[]`: a list is a
    /// collection built from its elements, so nothing about it changes at two
    /// the way a paren becomes a tuple at two. The element list is an `ARG_LIST`
    /// for the reason a subscript's is — the comma rules, the trailing comma and
    /// the recovery are one loop, and a second copy is a second place for a rule
    /// to be missing.
    fn parse_list(&mut self) {
        self.start_node(SyntaxKind::LIST_EXPR);
        self.bump(); // `[`
                     // Unlike a subscript, an empty one is legal: `[]` is the empty
                     // `Vec`, whose element type inference takes from its use.
        self.parse_arg_list_until(SyntaxKind::R_BRACK, "`]` to close list literal");
        self.finish_node(); // LIST_EXPR
    }

    fn parse_if(&mut self) {
        self.start_node(SyntaxKind::IF_EXPR);
        self.bump(); // `if`
                     // The condition is a parenthesized or bare expression; accept either.
                     // Suppress record-literal parsing so `if x { … }` doesn't read
                     // the then-block as `x { … }`.
        self.parse_expr_no_struct_lit();
        self.parse_block(); // then-branch
        if self.eat(SyntaxKind::KW_ELSE) {
            self.start_node(SyntaxKind::ELSE_BRANCH);
            if self.at(SyntaxKind::KW_IF) {
                self.parse_if();
            } else {
                self.parse_block();
            }
            self.finish_node();
        }
        self.finish_node();
    }

    fn parse_while(&mut self) {
        self.start_node(SyntaxKind::WHILE_EXPR);
        self.bump(); // `while`
        self.parse_expr_no_struct_lit();
        self.parse_block();
        self.finish_node();
    }

    /// `for pattern in iter { body }` (§4.11). The binding and the `in` keyword
    /// separate the iterator expression from the loop body.
    fn parse_for(&mut self) {
        self.start_node(SyntaxKind::FOR_EXPR);
        self.bump(); // `for`
                     // The binding is a **pattern**: `for (k, v) in m` takes the
                     // pair apart, and a bare name is the pattern that binds the
                     // whole item. Nothing here can be confused with the loop
                     // body — the pattern is followed by `in`, never by `{`.
        self.parse_pattern();
        self.expect(SyntaxKind::KW_IN, "`in` after the for-loop binding");
        self.parse_expr_no_struct_lit(); // iterator
        self.parse_block();
        self.finish_node();
    }

    /// `loop { body }` (§4.11) — an explicit infinite loop, terminated by
    /// `break` (optionally with a value).
    fn parse_loop(&mut self) {
        self.start_node(SyntaxKind::LOOP_EXPR);
        self.bump(); // `loop`
        self.parse_block();
        self.finish_node();
    }

    /// `break [expr]` (§4.11). The optional value is an expression; absent
    /// means the loop yields Unit.
    fn parse_break(&mut self) {
        self.start_node(SyntaxKind::BREAK_EXPR);
        self.bump(); // `break`
                     // A value follows iff the next token starts an expression (not `;`/`}`/EOF).
        if self.starts_expr() {
            self.parse_expr();
        }
        self.finish_node();
    }

    /// `continue` (§4.11).
    fn parse_continue(&mut self) {
        self.start_node(SyntaxKind::CONTINUE_EXPR);
        self.bump(); // `continue`
        self.finish_node();
    }

    /// `return [expr]` (§4.11).
    fn parse_return(&mut self) {
        self.start_node(SyntaxKind::RETURN_EXPR);
        self.bump(); // `return`
        if self.starts_expr() {
            self.parse_expr();
        }
        self.finish_node();
    }

    /// `match scrutinee { pattern => expr, … }` (§4.6, §4.11).
    fn parse_match(&mut self) {
        self.start_node(SyntaxKind::MATCH_EXPR);
        self.bump(); // `match`
                     // The scrutinee is an expression; suppress record literals so the `{`
                     // opening the arm list isn't consumed as a record body.
        self.parse_expr_no_struct_lit();
        self.expect(SyntaxKind::L_BRACE, "`{` to begin match arms");
        if !self.at(SyntaxKind::R_BRACE) {
            loop {
                let before = self.meaningful_index();
                self.start_node(SyntaxKind::MATCH_ARM);
                self.parse_pattern();
                self.expect(SyntaxKind::FAT_ARROW, "`=>` in match arm");
                // Arm body: a record literal is legal here. The `{` that could be
                // confused with a block belongs to the *match*, and it was
                // consumed above — inside the arm list there is nothing left for
                // a `Name { … }` to be mistaken for.
                self.parse_expr();
                self.finish_node(); // MATCH_ARM
                                    // Arms are comma-OR-newline separated (§4.6).
                let comma = self.eat(SyntaxKind::COMMA);
                // Stop if we hit `}` or something that can't start a pattern.
                if self.at(SyntaxKind::R_BRACE) || !is_pattern_start(self.peek()) {
                    break;
                }
                if !comma && !self.newline_before() {
                    let span = self.current_span();
                    self.error_with(
                        DiagCode::ExpectedStatementSeparator,
                        span,
                        "expected `,` or a line break between match arms",
                    );
                }
                self.ensure_progress(before);
            }
        }
        self.expect(SyntaxKind::R_BRACE, "`}` to end match arms");
        self.finish_node(); // MATCH_EXPR
    }

    /// Parse a pattern (§4.6). Grammar:
    ///
    /// ```text
    /// pattern := "_"                                  // wildcard
    ///          | literal                              // `SyntaxKind::is_pattern_literal`
    ///          | Ident                                 // variable bind or payload-less variant
    ///          | Ident "(" [pattern ("," pattern)*] ")" // enum variant
    ///          | Ident "{" [pattern_field ("," pattern_field)*] "}" // record (§4.5)
    ///          | "{" pattern_field ("," pattern_field)* "}"        // headless record (ADR-091)
    ///          | "(" pattern ("," pattern)* ")"        // tuple (§4.4)
    /// pattern_field := Ident [":" pattern]
    /// ```
    ///
    /// A record pattern's `{` is unambiguous where a record *literal*'s is not:
    /// a pattern is followed by `=>` or `in`, never by a block, so nothing else
    /// can be waiting for that brace. That is also what makes the **head
    /// optional** (ADR-091 Decision 2): a leading `{` in pattern position can
    /// only ever open fields, so a headless record pattern needs no new token to
    /// tell it apart, and it pins its record from the scrutinee exactly as a
    /// tuple pattern does. It is the form a `choice(...)` payload record wants,
    /// because an anonymous record has no name a head could write.
    ///
    /// Parentheses in pattern position are **always** a tuple — there is no
    /// grouping form, because a pattern has no precedence to override. `(p)` is
    /// therefore a one-element tuple pattern, which `Y123` reports against every
    /// type: `TypeData::Tuple` carries two elements or more.
    ///
    /// A headless `{}` is rejected for `()`'s reason (ADR-091 Decision 3): it
    /// binds nothing and tests nothing against a record it cannot even name, so
    /// it is an irrefutable arm written by accident. The pattern that matches
    /// anything is spelled `_`. A *headed* `P {}` is kept — it names the record
    /// it tests for, so it is refutable, and it is `Some` beside `Some(_)`.
    fn parse_pattern(&mut self) {
        self.start_node(SyntaxKind::PATTERN);
        match self.peek() {
            SyntaxKind::UNDERSCORE => {
                self.bump(); // `_`
            }
            k if k.is_pattern_literal() => {
                self.bump(); // literal
            }
            // An interpolated literal is not a constant, so it is not a pattern
            // (§8.1, ADR-147). It is refused *here*, with the whole run consumed
            // into a `PARSE_ERROR`, rather than falling through to the arm below:
            // `match s { "{x}" => … }` would otherwise leave a `PATTERN` whose
            // only direct `Ident` is the hole's `x`, and `Pattern::kind` would
            // read that as a **variable bind** — an irrefutable arm that swallows
            // every value.
            SyntaxKind::InterpOpen => {
                let span = self.current_span();
                self.error(
                    span,
                    "an interpolated text literal is not a pattern; a pattern tests a constant",
                );
                self.start_node(SyntaxKind::PARSE_ERROR);
                self.parse_interp();
                self.finish_node();
            }
            SyntaxKind::Ident => {
                self.bump(); // variant name, record name, or variable bind
                if self.at(SyntaxKind::L_BRACE) {
                    // Record pattern: `Name { field, field: pat, … }`.
                    self.parse_record_pattern_fields();
                } else if self.eat(SyntaxKind::L_PAREN) {
                    // Enum variant with payload: `Name(pat, pat, …)`.
                    self.parse_pattern_list(SyntaxKind::R_PAREN);
                    self.expect(SyntaxKind::R_PAREN, "`)` to close variant pattern");
                }
            }
            SyntaxKind::L_BRACE => {
                // Headless record pattern `{ a, b: p }` (ADR-091). The fields
                // are the headed form's, unchanged — one production, so `for
                // {x, y} in points` and `|{x, y}| x + y` arrive with it.
                if self.nth_kind(1) == SyntaxKind::R_BRACE {
                    // `{}` binds nothing and names no record: an arm nobody can
                    // read as refutable. Reported where `()` is, and for the
                    // same reason.
                    let span = self.current_span();
                    self.error(span, "expected a pattern");
                }
                self.parse_record_pattern_fields();
            }
            SyntaxKind::L_PAREN => {
                // Tuple pattern `(a, b)` — or a grouping `(p)`, which the list
                // leaves as the one child it parsed.
                self.bump(); // `(`
                if self.at(SyntaxKind::R_PAREN) {
                    // `()` has no type to match: `Unit` is not a tuple.
                    let span = self.current_span();
                    self.error(span, "expected a pattern");
                } else {
                    self.parse_pattern_list(SyntaxKind::R_PAREN);
                }
                self.expect(SyntaxKind::R_PAREN, "`)` to close tuple pattern");
            }
            _ => {
                let span = self.current_span();
                self.error(span, "expected a pattern");
                if !self.at_end() {
                    self.bump();
                }
            }
        }
        self.finish_node(); // PATTERN
    }

    /// A comma-separated list of patterns, up to but not including `closer`.
    /// A trailing comma closes the list rather than opening an element.
    fn parse_pattern_list(&mut self, closer: SyntaxKind) {
        loop {
            let before = self.meaningful_index();
            self.parse_pattern();
            if !self.eat(SyntaxKind::COMMA) {
                break;
            }
            if self.at(closer) {
                break;
            }
            self.ensure_progress(before);
        }
    }

    /// The `{ field, field: pat, … }` body of a record pattern (§4.5).
    /// A punned field binds the field's own name; an explicit one matches the
    /// sub-pattern against that field.
    fn parse_record_pattern_fields(&mut self) {
        self.bump(); // `{`
        if !self.at(SyntaxKind::R_BRACE) {
            loop {
                let before = self.meaningful_index();
                self.start_node(SyntaxKind::PATTERN_FIELD);
                self.expect(SyntaxKind::Ident, "field name");
                if self.eat(SyntaxKind::COLON) {
                    self.parse_pattern();
                }
                self.finish_node(); // PATTERN_FIELD
                if !self.eat(SyntaxKind::COMMA) {
                    break;
                }
                // A trailing comma closes the list.
                if self.at(SyntaxKind::R_BRACE) {
                    break;
                }
                self.ensure_progress(before);
            }
        }
        self.expect(SyntaxKind::R_BRACE, "`}` to close record pattern");
    }

    // --- types --------------------------------------------------------------

    /// Parse a type annotation. Grammar:
    ///
    /// ```text
    /// type := atom_type ("->" type)?       // function types, right-assoc
    /// atom_type := Ident                   // scalar: Int, Text, Bool, ...
    ///            | "(" [type ("," type)*] ")"  // tuple (≥2) or grouped type
    /// ```
    ///
    /// A scalar or grouped type becomes a [`TYPE_REF`](SyntaxKind::TYPE_REF); a
    /// parenthesized two-or-more-element list becomes a
    /// [`TUPLE_TYPE`](SyntaxKind::TUPLE_TYPE); anything followed by `->` wraps in
    /// an [`FN_TYPE`](SyntaxKind::FN_TYPE). Unknown identifiers (e.g. a typo or a
    /// reserved-but-unused scalar like `Float`) parse as `TYPE_REF` and are
    /// rejected by name resolution (`N002`), not by the parser.
    fn parse_type(&mut self) {
        let cp = self.checkpoint_lhs();
        self.parse_atom_type();
        // Function types bind right-associatively: `A -> B -> C` = `A -> (B -> C)`.
        if self.eat(SyntaxKind::THIN_ARROW) {
            self.parse_type(); // rhs (recurses, so right-assoc)
                               // Wrap lhs + arrow + rhs retroactively. `parse_atom_type` already
                               // emitted exactly one node; reopening at `cp` captures it.
            self.start_node_at(cp, SyntaxKind::FN_TYPE);
            self.finish_node();
        }
        // A scalar atom is already a TYPE_REF; a tuple/group is TUPLE_TYPE; an
        // arrow-wrapped one is FN_TYPE. The node is on the builder.
    }

    /// Parse one atomic type (no `->`). Emits exactly one node onto the builder:
    /// [`TYPE_REF`] for a scalar or grouped type, [`TUPLE_TYPE`] for two or more
    /// comma-separated elements. A scalar name followed by `[T]` or `[K, V]` is
    /// a collection type (§4.4) — also emitted as `TYPE_REF` with the bracketed
    /// args as children.
    fn parse_atom_type(&mut self) {
        if self.at(SyntaxKind::Ident) {
            let cp = self.checkpoint_lhs();
            self.eat_trivia();
            self.start_node(SyntaxKind::TYPE_REF);
            self.bump_meaningful(); // the scalar name
            self.finish_node();
            // Collection type args: `Vec[Int]`, `Map[Text, Int]`, …
            if self.at(SyntaxKind::L_BRACK) {
                self.bump(); // `[`
                self.start_node_at(cp, SyntaxKind::TYPE_REF);
                // The first type arg.
                self.parse_type();
                while self.eat(SyntaxKind::COMMA) {
                    // A trailing comma closes the list.
                    if self.at(SyntaxKind::R_BRACK) {
                        break;
                    }
                    self.parse_type();
                }
                self.expect(SyntaxKind::R_BRACK, "`]`");
                self.finish_node(); // wraps name + args into one TYPE_REF
            }
            return;
        }
        if self.at(SyntaxKind::L_PAREN) {
            // `( T )` (grouped) or `( T, U, … )` (tuple) — the same list, the
            // same arity rule and the same retroactive node as `parse_paren`'s,
            // over `parse_type` instead of `parse_expr`.
            let cp = self.checkpoint_lhs();
            self.bump(); // `(`
            self.parse_parenthesized(
                cp,
                SyntaxKind::TUPLE_TYPE,
                SyntaxKind::TYPE_REF,
                Self::parse_type,
            );
            return;
        }
        // Nothing recognizable: emit a diagnostic + a PARSE_ERROR node, but make
        // progress (OOM rule) by consuming the stray token if any.
        if self.at_end() {
            let span = self.current_span();
            self.error(span, "expected a type");
        } else {
            self.start_node(SyntaxKind::PARSE_ERROR);
            let span = self.current_span();
            self.error(span, "expected a type");
            self.bump();
            self.finish_node();
        }
    }

    /// An identifier, possibly followed by a call `(args)` and/or `.method(args)`
    /// postfixes. Method calls chain left-associatively: `v.push(1).len()`.
    fn parse_name_or_call(&mut self, lit: StructLit) {
        let cp = self.checkpoint_lhs();
        // Peek the identifier text to special-case `parse(text, parser_expr)`
        // (§7.1) before committing to an ordinary call.
        let name_text = self.peek_text();
        // `parse(text, parser_expression)` (§7.1) is *syntax*, not a call of a
        // binding named `parse` — so the keyword must not become a `PATH_EXPR`.
        // Name resolution would report `` `parse` is not defined `` on every
        // use, and `ParseExpr::text_expr` — "the first `Expr` child" — would
        // answer with the keyword's own path instead of the text argument.
        // Decided before the node is opened, which needs one token of lookahead.
        if name_text == Some("parse") && self.nth_kind(1) == SyntaxKind::L_PAREN {
            self.start_node_at(cp, SyntaxKind::PARSE_EXPR);
            self.bump(); // `parse`
            self.bump(); // `(`
            self.parse_expr(); // first arg: the Text
            if self.eat(SyntaxKind::COMMA) {
                self.parse_parser_expr();
            }
            self.expect(SyntaxKind::R_PAREN, "`)` to close parse()");
            self.finish_node(); // PARSE_EXPR
            return;
        }
        // `Counter[(Int, Int)]()` — explicit type arguments on a constructor call
        // (§3.3). Decided from the *name* before the brackets are reached,
        // because the brackets themselves are a subscript's and their contents
        // cannot break the tie: `Int` parses as an expression too.
        //
        // Consequence, stated rather than hidden: a binding that shadows a type
        // constructor's name cannot be subscripted — `Counter[0]` reads as a type
        // argument list and then wants a `(`. That is the whole cost of the rule,
        // and it buys `m[k](7)` staying a call on an indexed closure.
        let takes_type_args = name_text.is_some_and(|t| TYPE_CONSTRUCTOR_NAMES.contains(&t))
            && self.nth_kind(1) == SyntaxKind::L_BRACK;
        self.start_node(SyntaxKind::PATH_EXPR);
        self.bump(); // name
        self.finish_node();
        if takes_type_args {
            self.parse_type_arg_list();
        }
        if self.at_argument_list() {
            self.bump(); // `(`
            self.start_node_at(cp, SyntaxKind::CALL_EXPR);
            // Re-open the path as the callee: rowan's checkpoint wraps the
            // already-emitted PATH_EXPR, so the call's first child is the path.
            self.parse_arg_list();
            self.finish_node(); // CALL_EXPR
        } else if self.at(SyntaxKind::L_BRACE) && lit == StructLit::Allowed {
            // Record literal: `Name { field: expr, … }` or `Name { x, y }` (§4.5
            // punning). In expression position, a bare name followed by `{` is a
            // record construction; the keyword heads suppress that reading (see
            // [`StructLit`]) so their block is not taken as a record body.
            self.start_node_at(cp, SyntaxKind::RECORD_LIT_EXPR);
            self.bump(); // `{`
            self.start_node(SyntaxKind::FIELD_LIST);
            if !self.at(SyntaxKind::R_BRACE) {
                loop {
                    let before = self.meaningful_index();
                    self.start_node(SyntaxKind::FIELD);
                    self.expect(SyntaxKind::Ident, "field name");
                    // Field punning (`{ x, y }`) or explicit (`{ x: expr }`).
                    if self.eat(SyntaxKind::COLON) {
                        self.parse_expr();
                    }
                    self.finish_node();
                    if !self.eat(SyntaxKind::COMMA) {
                        break;
                    }
                    // A trailing comma closes the list.
                    if self.at(SyntaxKind::R_BRACE) {
                        break;
                    }
                    self.ensure_progress(before);
                }
            }
            self.expect(SyntaxKind::R_BRACE, "`}` to close record literal");
            self.finish_node(); // FIELD_LIST
            self.finish_node(); // RECORD_LIT_EXPR
        }
        // The rest of the postfix chain — `.method(args)`, `.field` and further
        // `(args)` calls in any order.
        self.parse_postfix(cp);
    }

    // -----------------------------------------------------------------------
    // Input-parser expression grammar (§7).
    //
    // `parser_expr := atom | template | call`
    // Whitespace and indentation outside backticks are insignificant (§7.1).
    // -----------------------------------------------------------------------

    /// Parse a parser expression (§7 EBNF). Emits a `PARSER_EXPR` wrapper node
    /// around exactly one of: an atomic name, a backtick template, or a
    /// constructor call `name(args)`.
    fn parse_parser_expr(&mut self) {
        self.eat_trivia(); // whitespace outside backticks is insignificant (§7.1)
        let kind = self.peek();
        match kind {
            // An unterminated run is still *shaped* like a template, and the
            // lexer has already reported it (T002, ADR-094). Taking it here
            // rather than falling through to "expected a parser expression"
            // is what keeps one typo to one error: the alternative is a P001
            // and then an I000 about an interior nobody wrote.
            SyntaxKind::BacktickTemplate | SyntaxKind::UnterminatedBacktickTemplate => {
                self.parse_parser_template()
            }
            SyntaxKind::Ident => {
                // An identifier is either an atomic parser (`int`, `char`, …)
                // or a constructor call (`lines(P)`, `sep(s, P)`). Decide by the
                // presence of `(`.
                if self.nth_kind(1) == SyntaxKind::L_PAREN {
                    self.parse_parser_call();
                } else {
                    self.parse_parser_atom();
                }
            }
            _ => {
                // Nothing recognizable as a parser expression.
                let span = self.current_span();
                self.error(span, "expected a parser expression");
                self.start_node(SyntaxKind::PARSER_EXPR);
                self.start_node(SyntaxKind::PARSE_ERROR);
                if !self.at_end() {
                    self.bump(); // guaranteed progress
                }
                self.finish_node(); // PARSE_ERROR
                self.finish_node(); // PARSER_EXPR
            }
        }
    }

    /// Parse an atomic parser name: `int`, `char`, `word`, `text`, `rest`,
    /// `digit` (§7.4). The identifier is wrapped in `PARSER_EXPR > PARSER_ATOM`.
    fn parse_parser_atom(&mut self) {
        self.start_node(SyntaxKind::PARSER_EXPR);
        self.start_node(SyntaxKind::PARSER_ATOM);
        self.bump(); // the atomic name
        self.finish_node(); // PARSER_ATOM
        self.finish_node(); // PARSER_EXPR
    }

    /// Parse a backtick template as a parser expression (§7.2). The whole
    /// `BacktickTemplate` token is emitted as a `PARSER_TEMPLATE` child; its
    /// interior is re-scanned by `praxis-input-parser` later (in HIR). The
    /// template node is wrapped in `PARSER_EXPR`.
    fn parse_parser_template(&mut self) {
        self.start_node(SyntaxKind::PARSER_EXPR);
        self.start_node(SyntaxKind::PARSER_TEMPLATE);
        self.bump(); // the BacktickTemplate token (interior re-scanned in HIR)
        self.finish_node(); // PARSER_TEMPLATE
        self.finish_node(); // PARSER_EXPR
    }

    /// Parse a constructor call `name(args)` (§7.5). Emits
    /// `PARSER_EXPR > PARSER_CALL > PATH_EXPR + PARSER_ARG_LIST`. Each argument
    /// is one of:
    /// - a positional parser expression (`lines(int)` → child `int`);
    /// - a positional literal, emitted as a `LITERAL` node: a string (the
    ///   separator for `sep`, the set for `one_of`) or a whole number (the
    ///   count of `repeated(P, N)`, §7.5). **Which constructors accept which
    ///   literal is not the grammar's question** — it is
    ///   `Constructor::arg_shape`'s, exactly as `Constructor::keyword_arg`
    ///   owns the keyword question. A count written where no count belongs
    ///   earns an argument diagnostic naming the constructor, which says more
    ///   than "expected a parser expression" does;
    /// - a named argument `name: parser_expr` (§7.5), emitted as a
    ///   `PARSER_NAMED_ARG` node — used by heterogeneous `sections`
    ///   (`rules: lines(...)`), `chars`/`grid` keyword args (`skip: whitespace`,
    ///   `fill: value`), and the `repeated(...)` tail marker of `sections`.
    fn parse_parser_call(&mut self) {
        self.start_node(SyntaxKind::PARSER_EXPR);
        self.start_node(SyntaxKind::PARSER_CALL);
        // The constructor name as a path.
        self.start_node(SyntaxKind::PATH_EXPR);
        self.bump(); // constructor name
        self.finish_node();
        // Argument list.
        self.expect(SyntaxKind::L_PAREN, "`(` to open parser call arguments");
        self.start_node(SyntaxKind::PARSER_ARG_LIST);
        if !self.at(SyntaxKind::R_PAREN) {
            loop {
                self.eat_trivia();
                if self.at(SyntaxKind::TextLit) || self.at(SyntaxKind::IntLit) {
                    // A positional literal: `sep`'s separator, or the count of
                    // `repeated(P, N)`.
                    self.start_node(SyntaxKind::LITERAL);
                    self.bump();
                    self.finish_node();
                } else if self.at(SyntaxKind::MINUS) && self.nth_kind(1) == SyntaxKind::IntLit {
                    // A negative count is still a *count*, and it belongs in one
                    // `LITERAL` node so the shape check can say what is wrong
                    // with it. Left to `parse_parser_expr`, `repeated(P, -1)`
                    // would report "expected a parser expression" at the `-` —
                    // a complaint about the wrong thing entirely.
                    self.start_node(SyntaxKind::LITERAL);
                    self.bump(); // `-`
                    self.bump(); // the digits
                    self.finish_node();
                } else if self.at(SyntaxKind::Ident) && self.nth_kind(1) == SyntaxKind::COLON {
                    // A named argument `name: parser_expr`. The name is a bare
                    // ident followed by `:`. This does not conflict with a
                    // constructor call (`lines(...)`) because that has `(` at
                    // position 1, not `:`.
                    self.start_node(SyntaxKind::PARSER_NAMED_ARG);
                    self.bump(); // the name ident
                    self.eat_trivia();
                    self.expect(SyntaxKind::COLON, "`:` after a named argument");
                    self.eat_trivia();
                    self.parse_parser_named_arg_value();
                    self.finish_node(); // PARSER_NAMED_ARG
                } else {
                    self.parse_parser_expr();
                }
                self.eat_trivia();
                if !self.eat(SyntaxKind::COMMA) {
                    break;
                }
                // Allow a trailing comma: if the next token is `)`, stop.
                self.eat_trivia();
                if self.at(SyntaxKind::R_PAREN) {
                    break;
                }
            }
        }
        self.expect(SyntaxKind::R_PAREN, "`)` to close parser call arguments");
        self.finish_node(); // PARSER_ARG_LIST
        self.finish_node(); // PARSER_CALL
        self.finish_node(); // PARSER_EXPR
    }

    /// Parse the value after `name:` in a parser call's argument list.
    ///
    /// Two shapes live here, and only the grammar can tell them apart by
    /// looking:
    /// - a **literal** (`fill: 0`, `fill: "-"`) — the value of a keyword
    ///   argument, which is not a parser expression at all. It becomes a
    ///   `PARSER_KEYWORD_VALUE` node holding the raw token, and whether the
    ///   constructor actually has a keyword argument of that name is decided
    ///   later, by `Constructor::keyword_arg`, where that rule already lives.
    /// - anything else (`rules: lines(int)`, `skip: whitespace`) — a parser
    ///   expression.
    fn parse_parser_named_arg_value(&mut self) {
        if matches!(
            self.peek(),
            SyntaxKind::IntLit | SyntaxKind::FloatLit | SyntaxKind::TextLit
        ) {
            self.start_node(SyntaxKind::PARSER_KEYWORD_VALUE);
            self.bump(); // the literal token
            self.finish_node();
        } else {
            self.parse_parser_expr();
        }
    }

    // -----------------------------------------------------------------------
    // Error recovery.
    // -----------------------------------------------------------------------

    /// Expect a **binding position**: a name, or `_` for one the program is
    /// deliberately not naming (D7, ADR-049). Reports at the current token if
    /// neither is there.
    ///
    /// `var _ = f()`, `fn g(_)` and `|_| 0` are legal and introduce nothing —
    /// the AST's name accessors look for an `Ident`, so a wildcard binder is an
    /// absent name all the way down rather than a symbol called `_`.
    fn expect_binder(&mut self, what: &str) -> bool {
        if self.at(SyntaxKind::UNDERSCORE) {
            self.bump();
            return true;
        }
        self.expect(SyntaxKind::Ident, what)
    }

    fn expect(&mut self, kind: SyntaxKind, what: &str) -> bool {
        if self.at(kind) {
            self.bump();
            true
        } else {
            let span = self.current_span();
            let spelling = self.current_spelling();
            self.error(span, format!("expected {what}, found {spelling}"));
            false
        }
    }

    /// The span of the current meaningful token (or a zero-width span at EOF).
    fn current_span(&mut self) -> Span {
        self.eat_trivia();
        if self.cursor < self.tokens.len() {
            self.tokens[self.cursor].span
        } else {
            Span::at(BytePos::ZERO)
        }
    }

    /// A human-readable spelling of the current token, for diagnostics.
    fn current_spelling(&mut self) -> &'static str {
        match self.peek() {
            SyntaxKind::EOF => "end of file",
            _ => "unexpected token",
        }
    }

    /// Skip tokens until a plausible statement boundary (a brace, a statement
    /// keyword, or EOF), wrapping skipped tokens in a `PARSE_ERROR` node.
    fn recover_to_stmt_boundary(&mut self) {
        self.start_node(SyntaxKind::PARSE_ERROR);
        let span = self.current_span();
        self.error(span, "unexpected token, skipping to recover");
        while !self.at_end()
            && !self.at(SyntaxKind::R_BRACE)
            && !matches!(
                self.peek(),
                SyntaxKind::KW_VAR
                    | SyntaxKind::KW_FN
                    | SyntaxKind::KW_IF
                    | SyntaxKind::KW_WHILE
                    | SyntaxKind::KW_RETURN
            )
        {
            self.bump();
        }
        self.finish_node();
    }

    /// The builder checkpoint *before* the current operand was emitted.
    ///
    /// In a single-pass builder the checkpoint must be captured before the
    /// children are emitted, so we stash one at the start of each prefix/atom.
    ///
    /// The trivia in front of the operand is emitted **first**, for
    /// [`start_node`](Self::start_node)'s reason and by the same rule: a node
    /// retroactively opened here — a `BIN_EXPR`, a `RANGE_EXPR`, a `CALL_EXPR`,
    /// a parenthesized expression — would otherwise begin at a checkpoint taken
    /// before the whitespace and swallow it.
    fn checkpoint_lhs(&mut self) -> rowan::Checkpoint {
        self.eat_trivia();
        self.builder.checkpoint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use praxis_source::{DiagnosticCategory, SourceMap};

    fn parse_text(text: &str) -> ParseOutput {
        let map = SourceMap::new();
        let id = map.intern("test.px", text);
        parse(id, text)
    }

    fn dump(text: &str) -> String {
        praxis_test_support::format_syntax_tree(&parse_text(text).tree)
    }

    #[test]
    fn parses_var_binding() {
        let out = parse_text("var x = 1");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        insta::assert_snapshot!(dump("var x = 1"), @r#"
        SOURCE_FILE@0..9
          VAR_STMT@0..9
            KW_VAR "var"@0..3
            Whitespace " "@3..4
            Ident "x"@4..5
            Whitespace " "@5..6
            EQ "="@6..7
            Whitespace " "@7..8
            LITERAL@8..9
              IntLit "1"@8..9
        "#);
    }

    #[test]
    fn parses_var_binding_and_reassignment() {
        let out = parse_text("var score = 0");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::VAR_STMT));
    }

    #[test]
    fn parses_function_definition() {
        let src = "fn add(a: Int, b: Int) -> Int { a + b }";
        let out = parse_text(src);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::FN_ITEM));
        assert!(kinds.contains(&SyntaxKind::PARAM_LIST));
        assert!(kinds.contains(&SyntaxKind::PARAM));
        assert!(kinds.contains(&SyntaxKind::BLOCK_EXPR));
        assert!(kinds.contains(&SyntaxKind::BIN_EXPR));
    }

    #[test]
    fn parses_out_call() {
        let out = parse_text("out(42)");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::CALL_EXPR));
        assert!(kinds.contains(&SyntaxKind::ARG_LIST));
    }

    #[test]
    fn parses_if_else() {
        let src = "if x { out(1) } else { out(2) }";
        let out = parse_text(src);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::IF_EXPR));
        assert!(kinds.contains(&SyntaxKind::ELSE_BRANCH));
    }

    #[test]
    fn parses_while_loop() {
        let out = parse_text("while x < 10 { x = x + 1 }");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::WHILE_EXPR));
    }

    #[test]
    fn parses_block_expression() {
        let out = parse_text("{ var a = 1\n a }");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::BLOCK_EXPR));
        assert!(kinds.contains(&SyntaxKind::VAR_STMT));
        assert!(kinds.contains(&SyntaxKind::EXPR_STMT));
    }

    #[test]
    fn parses_text_literal() {
        let out = parse_text(r#"out("hello")"#);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::TextLit));
    }

    /// A `'#'` in expression position is a `LITERAL` holding one `CharLit`, and
    /// the node spans exactly the literal — no leading trivia.
    #[test]
    fn parses_char_literal() {
        let src = "var c = '#'";
        let out = parse_text(src);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let lit = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::LITERAL)
            .expect("a LITERAL node");
        assert_eq!(lit.text().to_string(), "'#'");
        assert!(construct_names(&out.tree).contains(&SyntaxKind::CharLit));
    }

    // --- string interpolation (§8.1, ADR-147) -------------------------------

    /// An interpolated literal is an `INTERP_EXPR`, and the hole's expression is
    /// an ordinary subtree under it — a `PATH_EXPR` here, a `BIN_EXPR` below.
    /// That is what everything else in the compiler reads it through.
    #[test]
    fn parses_an_interpolated_literal() {
        let out = parse_text(r#"out("Part 2: {part2}")"#);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::INTERP_EXPR));
        assert!(kinds.contains(&SyntaxKind::PATH_EXPR));
        assert!(kinds.contains(&SyntaxKind::InterpOpen));
        assert!(kinds.contains(&SyntaxKind::InterpClose));
        // And it is *not* a LITERAL: a LITERAL is a leaf whose value comes off a
        // token, which an interpolated literal's does not.
        assert!(!kinds.contains(&SyntaxKind::TextLit));
    }

    /// A hole holds a full expression (ADR-147 decision 1), so the shapes below
    /// each land as the node they would be anywhere else.
    #[test]
    fn a_hole_holds_a_full_expression() {
        for (src, expected) in [
            (r#"out("{a + b}")"#, SyntaxKind::BIN_EXPR),
            (r#"out("{p.0}")"#, SyntaxKind::TUPLE_INDEX_EXPR),
            (r#"out("{xs.len()}")"#, SyntaxKind::METHOD_CALL_EXPR),
            (r#"out("{m["k"]}")"#, SyntaxKind::INDEX_EXPR),
            (r#"out("{if c { 1 } else { 2 }}")"#, SyntaxKind::IF_EXPR),
        ] {
            let out = parse_text(src);
            assert!(out.diagnostics.is_empty(), "{src}: {:?}", out.diagnostics);
            assert!(construct_names(&out.tree).contains(&expected), "{src}");
        }
    }

    /// Several holes are one node with one `InterpClose`, and the fragments
    /// alternate with the holes.
    #[test]
    fn several_holes_are_one_node() {
        let out = parse_text(r#"out("{a} and {b} and {c}")"#);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert_eq!(
            kinds
                .iter()
                .filter(|k| **k == SyntaxKind::INTERP_EXPR)
                .count(),
            1
        );
        assert_eq!(
            kinds
                .iter()
                .filter(|k| **k == SyntaxKind::InterpMiddle)
                .count(),
            2
        );
    }

    /// Postfix operators apply to the whole literal, not to the last hole: an
    /// `INTERP_EXPR` is an atom like any other.
    #[test]
    fn an_interpolated_literal_takes_postfix_operators() {
        let out = parse_text(r#"var n = "{a}b".len()"#);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert!(construct_names(&out.tree).contains(&SyntaxKind::METHOD_CALL_EXPR));
    }

    /// **An interpolated literal is not a pattern**, and refusing it is not
    /// pedantry: without the arm that reports it, `match s { "{x}" => … }`
    /// leaves a `PATTERN` whose only direct `Ident` is the hole's `x`, which
    /// `Pattern::kind` reads as a variable bind — an irrefutable arm that
    /// swallows every value with no diagnostic at all. That is why this asserts
    /// the diagnostic rather than the tree shape.
    #[test]
    fn an_interpolated_literal_is_not_a_pattern() {
        let out = parse_text("var r = match s {\n    \"{x}\" => 1\n    _ => 2\n}");
        assert!(
            !out.diagnostics.is_empty(),
            "an interpolated literal in pattern position must be reported"
        );
        assert!(
            out.diagnostics
                .iter()
                .any(|d| d.message().contains("not a pattern")),
            "{:?}",
            out.diagnostics
        );
    }

    /// `{{` is the escape in Rust, C# and Python, so it is the first thing a
    /// reader tries — and here it would otherwise *parse*: `{` opens the hole,
    /// `{}` is an empty block, `}` closes it, and `"a{{}}b"` prints `aUnitb`
    /// with no diagnostic. The refusal is what keeps that from being silent.
    #[test]
    fn a_doubled_brace_is_refused_rather_than_meaning_a_block() {
        for src in [
            "out(\"a{{}}b\")",
            "out(\"a{{x}}b\")",
            "out(\"{{}}\")",
            "out(\"ok {n} then a{{}}b\")",
        ] {
            let out = parse_text(src);
            assert!(
                out.diagnostics
                    .iter()
                    .any(|d| d.message().contains("is not an escape for a literal brace")),
                "`{src}` must be refused, got {:?}",
                out.diagnostics
            );
        }
        // The spelling that works, and an ordinary hole, are both untouched.
        for src in ["out(\"a\\{\\}b\")", "out(\"a{n}b\")", "out(\"{a + b}\")"] {
            let out = parse_text(src);
            assert!(
                out.diagnostics.is_empty(),
                "`{src}` must parse cleanly, got {:?}",
                out.diagnostics
            );
        }
    }

    /// **`is_pattern_start`'s half of ADR-141.** Leave `CharLit` out of it and
    /// the arm list stops after `'#' => …`: the second and third arms leave the
    /// tree with no diagnostic at all, and the literal still looks like it
    /// works.
    #[test]
    fn a_char_arm_does_not_truncate_the_arm_list() {
        let src = "var r = match c {\n    '#' => 1\n    '.' => 2\n    _ => 3\n}";
        let out = parse_text(src);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let arms = construct_names(&out.tree)
            .into_iter()
            .filter(|k| *k == SyntaxKind::MATCH_ARM)
            .count();
        assert_eq!(arms, 3);
    }

    // --- statement separation and postfix chains ---------------------------

    #[test]
    fn regression_same_line_statements_require_a_semicolon() {
        let out = parse_text("var a = 1 var b = 2");
        assert!(
            !out.diagnostics.is_empty(),
            "two statements on one line must not parse as if a separator existed"
        );
    }

    #[test]
    fn regression_semicolons_separate_top_level_statements() {
        let out = parse_text("var a = 1; var b = 2");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert_eq!(
            construct_names(&out.tree)
                .iter()
                .filter(|kind| **kind == SyntaxKind::VAR_STMT)
                .count(),
            2
        );
    }

    #[test]
    fn regression_newline_terminates_a_bare_return() {
        let out = parse_text("fn f() { return\n1 }");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let return_expr = out
            .tree
            .descendants()
            .find(|node| node.kind() == SyntaxKind::RETURN_EXPR)
            .expect("return expression");
        assert_eq!(
            return_expr.children().count(),
            0,
            "the next line must be a separate expression, not return's value"
        );
    }

    #[test]
    fn regression_postfix_forms_may_be_interleaved() {
        let out = parse_text("(fs).get(0)(100)");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == SyntaxKind::CALL_EXPR)
                .count(),
            1,
            "the final `(100)` must call the result of `.get(0)`"
        );
        assert_eq!(
            out.tree.children().count(),
            1,
            "the postfix call must remain part of the same expression statement"
        );
    }

    // --- where a newline ends a statement, and where it does not -----------

    /// D8's second half, stated for the operator it is most likely to break.
    /// A newline is consulted between statements and at `break`/`return`'s
    /// optional-value decision — never inside the Pratt loop.
    #[test]
    fn an_operator_continues_across_a_line_break() {
        let out = parse_text("var a = 1 +\n2");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == SyntaxKind::VAR_STMT)
                .count(),
            1,
            "`1 +` and `2` are one addition, not two statements"
        );
        assert!(kinds.contains(&SyntaxKind::BIN_EXPR));
    }

    /// **A node never begins with trivia**, so an expression's `text_range` is
    /// the expression and a caret under it underlines only that.
    ///
    /// Asserted as an invariant over whole trees rather than as another
    /// snapshot: `start_node` has dozens of call sites and a snapshot pins the
    /// handful a fixture happens to reach. The exception is the **root**, which
    /// is where a file's leading trivia has to go — there is no node before it.
    #[test]
    fn a_node_never_begins_with_trivia() {
        for src in [
            "var c = a + b",
            "  var c = ( a ) + 1  ",
            "// a leading comment\nvar t = true && false",
            "fn f(x) -> Int {\n    var y = -x\n    y * 2\n}",
            "var v = read lines( `{a:int},{b:int}` )",
            "var m = match t {\n    A => 1\n    _ => 2\n}",
            "for i in 0..n {\n    out( i )\n}",
            "var p = Point { x: 1, y: 2 }",
            "var f = |a, b| a + b\nvar g = f ( 1 , 2 )",
            "var s = 0\ns += grid [ 1 , 2 ]",
            // Recovery paths open nodes too, and `PARSE_ERROR` is a node.
            "var @ = 1\nvar ok = 2",
            "var x = (",
        ] {
            let tree = parse_text(src).tree;
            for node in tree.descendants() {
                if node.kind() == SyntaxKind::SOURCE_FILE {
                    continue;
                }
                let Some(first) = node.first_token() else {
                    continue;
                };
                assert!(
                    !first.kind().is_trivia(),
                    "{:?}@{:?} begins with {:?} in {src:?}\n{}",
                    node.kind(),
                    node.text_range(),
                    first.kind(),
                    praxis_test_support::format_syntax_tree(&tree),
                );
            }
        }
    }

    /// The same rule stated directly: an operand's range is the operand, so
    /// `lhs.syntax().text_range()` is what a diagnostic can point at.
    #[test]
    fn an_operands_range_is_the_operand_and_not_the_space_before_it() {
        // `var c = a + b` — `a` at 8..9, `b` at 12..13.
        let tree = parse_text("var c = a + b").tree;
        let bin = tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::BIN_EXPR)
            .expect("a BIN_EXPR");
        assert_eq!(
            bin.text_range(),
            rowan::TextRange::new(8.into(), 13.into()),
            "the binary expression is `a + b`, not `= a + b`"
        );
        let operands: Vec<_> = bin
            .children()
            .filter(|n| n.kind() == SyntaxKind::PATH_EXPR)
            .map(|n| n.text_range())
            .collect();
        assert_eq!(
            operands,
            vec![
                rowan::TextRange::new(8.into(), 9.into()),
                rowan::TextRange::new(12.into(), 13.into()),
            ],
            "each operand's range is one character wide"
        );
    }

    /// `..`/`..=` is an infix operator that builds a `RANGE_EXPR` (ADR-059), and
    /// it binds **looser than arithmetic and comparison**: every range in the
    /// corpus writes an arithmetic bound, so `0..n - 1` has to be `0..(n - 1)`.
    #[test]
    fn a_range_binds_looser_than_the_arithmetic_in_its_bounds() {
        // The bound is the whole subtraction, so the RANGE_EXPR contains a
        // BIN_EXPR rather than the other way round.
        insta::assert_snapshot!(dump("var r = 0..n - 1"), @r#"
        SOURCE_FILE@0..16
          VAR_STMT@0..16
            KW_VAR "var"@0..3
            Whitespace " "@3..4
            Ident "r"@4..5
            Whitespace " "@5..6
            EQ "="@6..7
            Whitespace " "@7..8
            RANGE_EXPR@8..16
              LITERAL@8..9
                IntLit "0"@8..9
              DOT2 ".."@9..11
              BIN_EXPR@11..16
                PATH_EXPR@11..12
                  Ident "n"@11..12
                Whitespace " "@12..13
                MINUS "-"@13..14
                Whitespace " "@14..15
                LITERAL@15..16
                  IntLit "1"@15..16
        "#);
        // …and looser than comparison too, so `a..b == c..d` compares two ranges.
        let out = parse_text("var b = 1..2 == 3..4");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert_eq!(
            kinds
                .iter()
                .filter(|k| **k == SyntaxKind::RANGE_EXPR)
                .count(),
            2,
            "two ranges, one comparison — not a range over a Bool"
        );
        assert_eq!(
            kinds.iter().filter(|k| **k == SyntaxKind::BIN_EXPR).count(),
            1
        );
    }

    /// `..=` is its own token in the same position, and the *node* is the same
    /// kind — the difference is the operator token, which is where
    /// `RangeExpr::is_inclusive` reads it from.
    #[test]
    fn an_inclusive_range_is_the_same_node_with_a_different_operator() {
        let out = parse_text("var r = 0..=9");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::RANGE_EXPR));
        assert!(kinds.contains(&SyntaxKind::DOT2EQ));
        assert!(!kinds.contains(&SyntaxKind::DOT2));
    }

    /// A range is an ordinary expression, so it appears wherever one may: a
    /// `for` header (where the `{` must not be read as a record literal), a
    /// call argument, and a parenthesized bound.
    #[test]
    fn a_range_is_legal_wherever_an_expression_is() {
        for src in [
            "for i in 0..n { out(i) }",
            "for i in 0..=n { out(i) }",
            "var r = (0 - 1)..(n + 1)",
            "out(0..3)",
            // The bound may itself be a call or a method call.
            "var r = 0..v.len()",
            "var r = abs(0 - 3)..max(1, 2)",
        ] {
            let out = parse_text(src);
            assert!(out.diagnostics.is_empty(), "{src:?}: {:?}", out.diagnostics);
            assert!(
                construct_names(&out.tree).contains(&SyntaxKind::RANGE_EXPR),
                "{src:?} produced no RANGE_EXPR"
            );
        }
        // `Range` in *type* position is a type name, not a range expression —
        // the annotation form D6 makes a first-class value worth having.
        let out = parse_text("fn f(r: Range) -> Range { r }");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert!(!construct_names(&out.tree).contains(&SyntaxKind::RANGE_EXPR));
    }

    /// D8's rule, applied to `..`: a newline after it continues the expression,
    /// exactly as one after `+` does. The Pratt loop never consults
    /// `newline_before`, and this is the test that says a range is not the
    /// exception.
    #[test]
    fn a_range_continues_across_a_line_break() {
        let out = parse_text("var r = 1 ..\n5");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == SyntaxKind::VAR_STMT)
                .count(),
            1,
            "`1 ..` and `5` are one range, not two statements"
        );
        assert!(kinds.contains(&SyntaxKind::RANGE_EXPR));
    }

    /// …and so does a postfix chain: the line break before `.len()` is inside an
    /// expression, so it terminates nothing.
    #[test]
    fn a_method_chain_continues_across_a_line_break() {
        let out = parse_text("var n = v\n  .len()");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert!(construct_names(&out.tree).contains(&SyntaxKind::METHOD_CALL_EXPR));
    }

    /// `break`'s half of the optional-value rule;
    /// `regression_newline_terminates_a_bare_return` covers `return`.
    #[test]
    fn a_newline_terminates_a_bare_break() {
        let out = parse_text("loop { break\n1 }");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let break_expr = out
            .tree
            .descendants()
            .find(|node| node.kind() == SyntaxKind::BREAK_EXPR)
            .expect("break expression");
        assert_eq!(break_expr.children().count(), 0);
    }

    /// A `;` separates statements *inside* a block — it is one of the two things
    /// that can, the other being a line break.
    #[test]
    fn a_semicolon_separates_two_statements_on_one_line_in_a_block() {
        let out = parse_text("fn f() { var a = 1; var b = 2 }");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert_eq!(
            construct_names(&out.tree)
                .iter()
                .filter(|kind| **kind == SyntaxKind::VAR_STMT)
                .count(),
            2
        );
    }

    /// A run-on is reported where it happens and parsing continues: three
    /// statements, two missing separators, three `VAR_STMT`s and no cascade.
    #[test]
    fn each_missing_separator_is_reported_once_and_parsing_continues() {
        let out = parse_text("var a = 1 var b = 2 var c = 3");
        let separator_errors = out
            .diagnostics
            .iter()
            .filter(|d| d.kind() == DiagCode::ExpectedStatementSeparator)
            .count();
        assert_eq!(separator_errors, 2, "{:?}", out.diagnostics);
        assert_eq!(
            construct_names(&out.tree)
                .iter()
                .filter(|kind| **kind == SyntaxKind::VAR_STMT)
                .count(),
            3
        );
    }

    /// The block loop demands one too, and the closing `}` is a separator in its
    /// own right — a trailing expression needs nothing after it.
    #[test]
    fn a_block_demands_a_separator_but_its_closing_brace_is_one() {
        let clean = parse_text("fn f() { var a = 1\n a }");
        assert!(clean.diagnostics.is_empty(), "{:?}", clean.diagnostics);

        let run_on = parse_text("fn f() { var a = 1 a }");
        assert!(
            run_on
                .diagnostics
                .iter()
                .any(|d| d.kind() == DiagCode::ExpectedStatementSeparator),
            "{:?}",
            run_on.diagnostics
        );
    }

    /// Match arms are comma-OR-newline separated, and a run-on is reported.
    #[test]
    fn match_arms_on_one_line_need_a_comma() {
        let commas = parse_text("fn f() { match x { A => 1, B => 2 } }");
        assert!(commas.diagnostics.is_empty(), "{:?}", commas.diagnostics);

        let newlines = parse_text("fn f() { match x {\n A => 1\n B => 2\n } }");
        assert!(
            newlines.diagnostics.is_empty(),
            "{:?}",
            newlines.diagnostics
        );

        let run_on = parse_text("fn f() { match x { A => 1 B => 2 } }");
        assert!(
            run_on
                .diagnostics
                .iter()
                .any(|d| d.kind() == DiagCode::ExpectedStatementSeparator),
            "{:?}",
            run_on.diagnostics
        );
    }

    #[test]
    fn regression_parenthesized_record_literal_is_valid_in_a_condition() {
        let out = parse_text("if (Point { x: 1 } == p) { 0 }");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert!(
            construct_names(&out.tree).contains(&SyntaxKind::RECORD_LIT_EXPR),
            "the parenthesized condition must retain its record literal"
        );
    }

    #[test]
    fn regression_match_arm_may_return_a_record_literal() {
        let out = parse_text("match x { A => Point { x: 1 } }");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert!(
            construct_names(&out.tree).contains(&SyntaxKind::RECORD_LIT_EXPR),
            "the arm body must contain a record literal"
        );
    }

    // --- which brackets reset record-literal suppression --------------------

    /// Every bracketed context resets it, not only parentheses: an argument
    /// list, a tuple, and a block are all places where the `{` cannot be the
    /// body a keyword is waiting for.
    #[test]
    fn every_bracket_restores_record_literals_inside_a_condition() {
        for src in [
            "if f(Point { x: 1 }) { 0 }",
            "if (Point { x: 1 }, 2) == t { 0 }",
            "if { var p = Point { x: 1 }\n p.ok } { 0 }",
            "while v.has(Point { x: 1 }) { 0 }",
            "for q in near(Origin { x: 0 }) { 0 }",
            "match f(Point { x: 1 }) { A => 1 }",
        ] {
            let out = parse_text(src);
            assert!(out.diagnostics.is_empty(), "{src}: {:?}", out.diagnostics);
            assert!(
                construct_names(&out.tree).contains(&SyntaxKind::RECORD_LIT_EXPR),
                "{src}: the bracketed record literal was suppressed"
            );
        }
    }

    /// A match arm body resets it at every depth, not just at the top: the arm's
    /// own blocks and closures allow record literals too.
    #[test]
    fn a_match_arm_allows_a_record_literal_at_any_depth() {
        for src in [
            "match x { A => { Point { x: 1 } } }",
            "match x { A => |q| Point { x: 1 } }",
            "match x { A => if c { Point { x: 1 } } else { q } }",
        ] {
            let out = parse_text(src);
            assert!(out.diagnostics.is_empty(), "{src}: {:?}", out.diagnostics);
            assert!(
                construct_names(&out.tree).contains(&SyntaxKind::RECORD_LIT_EXPR),
                "{src}: the arm body suppressed its record literal"
            );
        }
    }

    /// …and the suppression still does the job it exists for, in all four heads
    /// and through the operands of the expression they own.
    #[test]
    fn a_keyword_head_still_claims_its_brace_as_a_block() {
        for (src, body) in [
            ("if p { 0 }", SyntaxKind::IF_EXPR),
            ("if a == p { 0 }", SyntaxKind::IF_EXPR),
            ("if !p { 0 }", SyntaxKind::IF_EXPR),
            ("while p { 0 }", SyntaxKind::WHILE_EXPR),
            ("for q in ps { 0 }", SyntaxKind::FOR_EXPR),
            ("match p { A => 1 }", SyntaxKind::MATCH_EXPR),
        ] {
            let out = parse_text(src);
            assert!(out.diagnostics.is_empty(), "{src}: {:?}", out.diagnostics);
            let kinds = construct_names(&out.tree);
            assert!(kinds.contains(&body), "{src}: expected a {body:?}");
            assert!(
                !kinds.contains(&SyntaxKind::RECORD_LIT_EXPR),
                "{src}: the head's brace was eaten as a record body"
            );
        }
    }

    // --- Pratt precedence ---

    #[test]
    fn arithmetic_is_left_associative() {
        // 1 + 2 + 3 should nest left: ((1 + 2) + 3).
        let out = parse_text("1 + 2 + 3");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let tree = dump("1 + 2 + 3");
        insta::assert_snapshot!(tree, @r#"
        SOURCE_FILE@0..9
          EXPR_STMT@0..9
            BIN_EXPR@0..9
              BIN_EXPR@0..5
                LITERAL@0..1
                  IntLit "1"@0..1
                Whitespace " "@1..2
                PLUS "+"@2..3
                Whitespace " "@3..4
                LITERAL@4..5
                  IntLit "2"@4..5
              Whitespace " "@5..6
              PLUS "+"@6..7
              Whitespace " "@7..8
              LITERAL@8..9
                IntLit "3"@8..9
        "#);
    }

    #[test]
    fn multiplication_binds_tighter_than_addition() {
        // 1 + 2 * 3 -> 1 + (2 * 3)
        let out = parse_text("1 + 2 * 3");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::BIN_EXPR));
    }

    #[test]
    fn parentheses_override_precedence() {
        // (1 + 2) * 3 -> the addition is the lhs of the multiply.
        let out = parse_text("(1 + 2) * 3");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::PAREN_EXPR));
    }

    #[test]
    fn parses_unary_minus() {
        let out = parse_text("-x");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::UNARY_EXPR));
    }

    // --- Error recovery (§15.2, acceptance: multiple diagnostics, never panic) ---

    #[test]
    fn malformed_input_produces_multiple_diagnostics_and_never_panics() {
        // A file with several distinct problems: missing `=` in a let, a stray
        // `)`, and a second broken statement. Recovery must keep going and emit
        // at least two P0xx diagnostics.
        let src = "var x 1\n )\nvar = \n";
        let out = parse_text(src);
        let parse_diags: Vec<_> = out
            .diagnostics
            .iter()
            .filter(|d| d.code().category() == DiagnosticCategory::Parse)
            .collect();
        assert!(
            parse_diags.len() >= 2,
            "expected >=2 parse diagnostics, got {}: {:?}",
            parse_diags.len(),
            parse_diags
        );
        // The tree is always produced.
        assert_eq!(out.tree.kind(), SyntaxKind::SOURCE_FILE);
    }

    #[test]
    fn empty_input_parses_to_empty_source_file() {
        let out = parse_text("");
        assert!(out.diagnostics.is_empty());
        assert_eq!(out.tree.kind(), SyntaxKind::SOURCE_FILE);
        // No children except possibly trailing trivia (none here).
        assert_eq!(out.tree.children_with_tokens().count(), 0);
    }

    #[test]
    fn whitespace_only_input_is_clean() {
        let out = parse_text("   \n  // just a comment\n  ");
        assert!(out.diagnostics.is_empty());
    }

    // --- type annotations + tuples ------------------------------------------

    #[test]
    fn parses_let_with_tuple_type_annotation() {
        let src = "var p: (Int, Int) = (1, 2)";
        let out = parse_text(src);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::TUPLE_TYPE));
        assert!(kinds.contains(&SyntaxKind::TUPLE_EXPR));
        assert!(kinds.contains(&SyntaxKind::TYPE_REF));
    }

    #[test]
    fn parses_fn_with_full_annotations() {
        // Full parameter + return annotations, including a tuple return type.
        let src = "fn f(a: Int, b: Int) -> (Int, Int) { (a, b) }";
        let out = parse_text(src);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::TYPE_REF));
        assert!(kinds.contains(&SyntaxKind::TUPLE_TYPE));
    }

    #[test]
    fn parses_higher_order_function_type() {
        // `(Int) -> Int` as a parameter type.
        let src = "fn apply(f: (Int) -> Int, x: Int) -> Int { f(x) }";
        let out = parse_text(src);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::FN_TYPE));
    }

    #[test]
    fn parses_scalar_type_annotation() {
        let src = "var x: Int = 1";
        let out = parse_text(src);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::TYPE_REF));
    }

    #[test]
    fn tuple_expression_distinguishes_from_paren() {
        // Single-element paren stays PAREN_EXPR; two-element is TUPLE_EXPR.
        let single = construct_names(&parse_text("(1)").tree);
        assert!(single.contains(&SyntaxKind::PAREN_EXPR));
        assert!(!single.contains(&SyntaxKind::TUPLE_EXPR));

        let pair = construct_names(&parse_text("(1, 2)").tree);
        assert!(pair.contains(&SyntaxKind::TUPLE_EXPR));
        assert!(!pair.contains(&SyntaxKind::PAREN_EXPR));
    }

    /// **A tuple has two elements or more** (§4.4), so `(1,)` is refused *here*,
    /// at the comma — a one-element `TUPLE_EXPR` is a shape `TupleElems` cannot
    /// represent, so typing and lowering would disagree about the node.
    ///
    /// The node recovers as `PAREN_EXPR`, the grouping the author most likely
    /// meant, so nothing downstream sees the shape that does not exist.
    #[test]
    fn a_one_element_tuple_is_refused_at_the_comma() {
        let parsed = parse_text("var t = (1,)\n");
        assert_eq!(
            parsed.diagnostics.len(),
            1,
            "one report, at the comma: {:?}",
            parsed.diagnostics
        );
        assert!(
            parsed.diagnostics[0].message().contains("two elements"),
            "{}",
            parsed.diagnostics[0].message()
        );
        let kinds = construct_names(&parsed.tree);
        assert!(kinds.contains(&SyntaxKind::PAREN_EXPR));
        assert!(!kinds.contains(&SyntaxKind::TUPLE_EXPR));

        // A trailing comma at an arity the type *has* is punctuation, untouched.
        let trailing = parse_text("var t = (1, 2,)\n");
        assert!(
            trailing.diagnostics.is_empty(),
            "{:?}",
            trailing.diagnostics
        );
        assert!(construct_names(&trailing.tree).contains(&SyntaxKind::TUPLE_EXPR));

        // The same rule in type position.
        let annotated = parse_text("var t: (Int,) = 1\n");
        assert_eq!(
            annotated.diagnostics.len(),
            1,
            "{:?}",
            annotated.diagnostics
        );
        assert!(!construct_names(&annotated.tree).contains(&SyntaxKind::TUPLE_TYPE));
        assert!(parse_text("var t: (Int, Text,) = (1, \"a\")\n")
            .diagnostics
            .is_empty());
    }

    #[test]
    fn tuple_expression_snapshot() {
        insta::assert_snapshot!(dump("(1, 2)"), @r#"
        SOURCE_FILE@0..6
          EXPR_STMT@0..6
            TUPLE_EXPR@0..6
              L_PAREN "("@0..1
              LITERAL@1..2
                IntLit "1"@1..2
              COMMA ","@2..3
              Whitespace " "@3..4
              LITERAL@4..5
                IntLit "2"@4..5
              R_PAREN ")"@5..6
        "#);
    }

    #[test]
    fn function_type_right_associative() {
        // `A -> B -> C` parses as `A -> (B -> C)`: the outer FN_TYPE's result is
        // itself an FN_TYPE.
        let src = "var f: Int -> Text -> Bool = panic";
        let out = parse_text(src);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        // Two FN_TYPE nodes for the nested arrow.
        let fn_type_count = kinds.iter().filter(|k| **k == SyntaxKind::FN_TYPE).count();
        assert_eq!(fn_type_count, 2);
    }

    // --- input-parser expression syntax (§7) ---

    #[test]
    fn parses_read_atomic() {
        let out = parse_text("var v = read int");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::READ_EXPR));
        assert!(kinds.contains(&SyntaxKind::PARSER_EXPR));
        assert!(kinds.contains(&SyntaxKind::PARSER_ATOM));
    }

    #[test]
    fn parses_read_lines_of_int() {
        let out = parse_text("var v = read lines(int)");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::READ_EXPR));
        assert!(kinds.contains(&SyntaxKind::PARSER_CALL));
        // Nested atom inside the call's arg list.
        assert!(kinds.contains(&SyntaxKind::PARSER_ATOM));
        assert!(kinds.contains(&SyntaxKind::PARSER_ARG_LIST));
    }

    #[test]
    fn parses_read_nested_constructors() {
        // sections(lines(csv(int))) — whitespace outside backticks is
        // insignificant (§7.1 acceptance criterion 5).
        let out = parse_text("var v = read sections( lines( csv( int ) ) )");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        // Three nested PARSER_CALL nodes (sections, lines, csv).
        let call_count = kinds
            .iter()
            .filter(|k| **k == SyntaxKind::PARSER_CALL)
            .count();
        assert_eq!(call_count, 3);
    }

    #[test]
    fn parses_read_template() {
        let out = parse_text("var v = read lines(`{x:int},{y:int}`)");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::READ_EXPR));
        assert!(kinds.contains(&SyntaxKind::PARSER_CALL));
        assert!(kinds.contains(&SyntaxKind::PARSER_TEMPLATE));
        assert!(kinds.contains(&SyntaxKind::BacktickTemplate));
    }

    #[test]
    fn parses_read_grid() {
        let out = parse_text("solve(read grid(char))");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::READ_EXPR));
        assert!(kinds.contains(&SyntaxKind::PARSER_CALL));
    }

    #[test]
    fn parses_read_sep_with_string_literal() {
        let out = parse_text(r#"var v = read sep(" -> ", word)"#);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::READ_EXPR));
        assert!(kinds.contains(&SyntaxKind::PARSER_CALL));
        // The string-literal separator is inside the arg list.
        assert!(kinds.contains(&SyntaxKind::TextLit));
    }

    // --- named arguments in parser constructor calls (§7.5) ------------------

    #[test]
    fn parses_named_args_in_sections() {
        // heterogeneous `sections(rules: ..., updates: ...)` — two named args.
        let out = parse_text("var v = read sections(rules: lines(int), updates: lines(int))");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::PARSER_NAMED_ARG));
        // Two named args.
        let named_count = kinds
            .iter()
            .filter(|k| **k == SyntaxKind::PARSER_NAMED_ARG)
            .count();
        assert_eq!(named_count, 2);
    }

    #[test]
    fn parses_repeated_tail_in_sections() {
        // The `repeated(...)` tail marker of named sections.
        let out =
            parse_text("var v = read sections(draws: csv(int), boards: repeated(matrix(int)))");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::PARSER_NAMED_ARG));
    }

    /// **A count is a positional literal, and the grammar has a shape for it.**
    /// The `N` of `repeated(P, N)` is an integer in positional position; the
    /// keyword-value path that also accepts one (`fill: 0`) is reachable only
    /// after a `name:`, so the positional case needs its own arm.
    #[test]
    fn a_parser_call_takes_a_positional_count_literal() {
        let out = parse_text("var v = read sections(a: repeated(lines(int), 2), b: lines(int))");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(
            !kinds.contains(&SyntaxKind::PARSE_ERROR),
            "a count argument must not be an error node"
        );
        assert!(
            kinds.contains(&SyntaxKind::LITERAL),
            "the count is a LITERAL child of the argument list"
        );
    }

    /// A negative count is one `LITERAL`, not a `-` the parser complains about
    /// separately: the diagnostic a reader needs is about the *count*, and only
    /// a node that holds the whole thing can carry it there.
    #[test]
    fn a_negative_count_is_one_literal_not_a_parse_error() {
        let out = parse_text("var v = read sections(a: repeated(int, -1))");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(!kinds.contains(&SyntaxKind::PARSE_ERROR));
        assert_eq!(
            kinds.iter().filter(|k| **k == SyntaxKind::LITERAL).count(),
            1,
            "`-1` is one literal node, not two"
        );
    }

    #[test]
    fn parses_keyword_arg_in_chars() {
        // `chars(one_of(...), skip: whitespace)` — a positional arg followed by
        // a named keyword arg.
        let out = parse_text("var v = read chars(one_of(\"LR\"), skip: whitespace)");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::PARSER_NAMED_ARG));
    }

    #[test]
    fn named_arg_does_not_shadow_constructor_call() {
        // A constructor call argument (`lines(int)`) has `(` at position 1, so
        // it is NOT mistaken for a named arg. Only `ident:` is.
        let out = parse_text("var v = read sections(lines(int))");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(
            !kinds.contains(&SyntaxKind::PARSER_NAMED_ARG),
            "positional constructor-call arg must not parse as a named arg"
        );
    }

    #[test]
    fn parses_parse_call() {
        let out = parse_text("var v = parse(sample, lines(int))");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::PARSE_EXPR));
        assert!(kinds.contains(&SyntaxKind::PARSER_CALL));
    }

    #[test]
    fn parser_expression_whitespace_is_insignificant() {
        // The same parser expression laid out differently must produce the same
        // tree shape (modulo trivia). §7.1 acceptance criterion 5.
        let a = construct_names(&parse_text("read lines(int)").tree);
        let b = construct_names(&parse_text("read\n  lines(\n    int\n  )").tree);
        // Filter out trivia (whitespace) for the comparison.
        let filt = |ks: &[SyntaxKind]| -> Vec<SyntaxKind> {
            ks.iter().filter(|k| !k.is_trivia()).copied().collect()
        };
        assert_eq!(filt(&a), filt(&b));
    }
    /// `&&` is one token and one infix operator, and its precedence is the two
    /// facts that matter: tighter than `||`, looser than comparison.
    ///
    /// The whole precedence table is asserted below rather than just `&&`'s row,
    /// because a binding power is only meaningful relative to the others.
    #[test]
    fn logical_and_binds_tighter_than_or_and_looser_than_comparison() {
        // One token, not two `AMP`s — max-munch, as for `||`.
        let out = parse_text("var b = x && y");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::AMP2));
        assert!(!kinds.contains(&SyntaxKind::AMP), "`&&` is never two `&`s");

        // `a || b && c` is `a || (b && c)`: the outer BIN_EXPR is the `||`.
        assert_eq!(
            shape("var r = a || b && c"),
            shape("var r = a || (b && c)"),
            "&& binds tighter than ||"
        );
        assert_ne!(shape("var r = a || b && c"), shape("var r = (a || b) && c"));

        // `a == b && c == d` is `(a == b) && (c == d)` — §3.3's own shape, and
        // the reason `&&` must be looser than comparison.
        assert_eq!(
            shape("var r = a == b && c == d"),
            shape("var r = (a == b) && (c == d)")
        );

        // `!x && y` is `(!x) && y`: prefix stays above every infix operator.
        // §3.3 writes `!diagonals && …`.
        assert_eq!(shape("var r = !x && y"), shape("var r = (!x) && y"));
        assert_ne!(shape("var r = !x && y"), shape("var r = !(x && y)"));

        // The rest of the table: arithmetic binds tighter than comparison, `*`
        // than `+`, and unary minus than `*`.
        assert_eq!(shape("var r = a + b < c"), shape("var r = (a + b) < c"));
        assert_eq!(shape("var r = a + b * c"), shape("var r = a + (b * c)"));
        assert_eq!(shape("var r = -a * b"), shape("var r = (-a) * b"));
        // …and `..` binds looser than the arithmetic in its bounds (ADR-059).
        assert_eq!(shape("var r = 0..n - 1"), shape("var r = 0..(n - 1)"));
        assert_ne!(shape("var r = 0..n - 1"), shape("var r = (0..n) - 1"));
    }

    /// **A trailing comma closes a list; it does not open another element.**
    ///
    /// Asserted over every comma-separated list in the grammar rather than over
    /// the argument list alone: accepting a trailing comma is a property of the
    /// grammar, and each list loop is a separate place for it to be missing.
    #[test]
    fn a_trailing_comma_closes_a_list_rather_than_opening_an_element() {
        // The list, and the same list without the trailing comma: identical trees
        // once the comma token is out of the way, which is what "closes it" means.
        for (with, without) in [
            // Call arguments, in §3.3's own layout.
            (
                "var d = max(\n  abs(a),\n  abs(b),\n)",
                "var d = max(abs(a), abs(b))",
            ),
            ("var x = f(1,)", "var x = f(1)"),
            // Tuple literal, collection type arguments.
            ("var t = (1, 2,)", "var t = (1, 2)"),
            ("var v: Vec[Int,] = Vec()", "var v: Vec[Int] = Vec()"),
            (
                "var m: Map[Text, Int,] = Map()",
                "var m: Map[Text, Int] = Map()",
            ),
            // Declarations: struct fields, enum variants, an enum payload.
            (
                "struct P { x: Int, y: Int, }",
                "struct P { x: Int, y: Int }",
            ),
            ("enum E { A, B, }", "enum E { A, B }"),
            ("enum E { B(Int, Int,), }", "enum E { B(Int, Int) }"),
            // Function and closure parameters.
            (
                "fn add(a: Int, b: Int,) -> Int { a + b }",
                "fn add(a: Int, b: Int) -> Int { a + b }",
            ),
            ("var f = |a, b,| a + b", "var f = |a, b| a + b"),
            // Record literal fields, and match arms.
            (
                "struct P { x: Int }\nvar p = P { x: 1, }",
                "struct P { x: Int }\nvar p = P { x: 1 }",
            ),
            (
                "var r = match n { 1 => 1, _ => 0, }",
                "var r = match n { 1 => 1, _ => 0 }",
            ),
            // Subscript indices.
            ("var c = grid[x, y,]", "var c = grid[x, y]"),
        ] {
            let out = parse_text(with);
            assert!(out.diagnostics.is_empty(), "{with}: {:?}", out.diagnostics);
            let filt = |t: &str| -> Vec<SyntaxKind> {
                construct_names(&parse_text(t).tree)
                    .into_iter()
                    .filter(|k| !k.is_trivia() && *k != SyntaxKind::COMMA)
                    .collect()
            };
            assert_eq!(filt(with), filt(without), "{with}");
        }

        // …and a *leading* or doubled comma is still a mistake: the rule is that
        // a comma may precede the closer, not that commas are optional.
        for bad in ["var x = f(1,,2)", "var x = f(,1)", "var t = (1,,2)"] {
            let out = parse_text(bad);
            assert!(!out.diagnostics.is_empty(), "{bad} must still report");
        }
    }

    /// **A subscript is a postfix form like a call**, so it chains with the
    /// other two in any order — and a statement whose target is one is an
    /// assignment.
    #[test]
    fn a_subscript_is_a_postfix_form_and_can_be_an_assignment_target() {
        // Reads, at both arities, and chained with the other postfix forms in
        // every order — which is what one loop over all three buys.
        for src in [
            "var v = m[key]",
            "var c = grid[x, y]",
            "var n = m[a][b]",
            "var n = f(x)[0]",
            "var n = grid[x, y].len()",
            "var n = v[0].0",
            "var n = rows[i].len() + 1",
            "var n = m[k](7)",
        ] {
            let out = parse_text(src);
            assert!(out.diagnostics.is_empty(), "{src}: {:?}", out.diagnostics);
        }

        // A subscript wraps the *whole* preceding expression, so `f(x)[0]` is one
        // statement and not two.
        let out = parse_text("var n = f(x)[0]");
        assert_eq!(
            construct_names(&out.tree)
                .iter()
                .filter(|k| **k == SyntaxKind::INDEX_EXPR)
                .count(),
            1
        );

        // Assignment through a subscript, in every operator the grammar has.
        for src in [
            "m[key] = 1",
            "counts[key] += 1",
            "m[key] -= 1",
            "m[key] *= 2",
            "m[key] /= 2",
            "m[key] %= 2",
            "grid[x, y] = 7",
            "m[a][b] += 1",
        ] {
            let out = parse_text(src);
            assert!(out.diagnostics.is_empty(), "{src}: {:?}", out.diagnostics);
            assert_eq!(
                construct_names(&out.tree)
                    .iter()
                    .filter(|k| **k == SyntaxKind::PLACE_ASSIGN_STMT)
                    .count(),
                1,
                "{src} is one assignment statement"
            );
        }

        // A bare name target is an `ASSIGN_STMT` — a different node, with a
        // *token* target — so the two paths stay distinct.
        let out = parse_text("x += 1");
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::ASSIGN_STMT), "{kinds:?}");
        assert!(!kinds.contains(&SyntaxKind::PLACE_ASSIGN_STMT), "{kinds:?}");

        // Whether a target is a place is inference's answer and not the
        // parser's, so both kinds parse the same way: `p.x = 3` is a field store
        // (§4.5) and `f() = 3` names no storage at all (`Y021`), and the parser
        // wraps each of them without asking. This is the assertion that would
        // fail if the wrap were made conditional on the target's shape.
        for src in ["f() = 3", "p.x = 3", "v.len() += 1"] {
            let out = parse_text(src);
            assert!(out.diagnostics.is_empty(), "{src}: {:?}", out.diagnostics);
            assert_eq!(
                construct_names(&out.tree)
                    .iter()
                    .filter(|k| **k == SyntaxKind::PLACE_ASSIGN_STMT)
                    .count(),
                1,
                "{src}"
            );
        }

        // A `[` after a line break does not continue the expression before it:
        // a list literal begins with one, so the tie is broken by position the
        // way it is for `(`.
        let out = parse_text(
            "var n = m
[key]",
        );
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert_eq!(
            out.tree.children().count(),
            2,
            "a line-leading `[` starts a list rather than subscripting"
        );
        // …and a `[` on the same line still subscripts, which is every subscript
        // any program writes.
        let out = parse_text("var n = m[key]");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert_eq!(out.tree.children().count(), 1);

        // An unclosed subscript is reported rather than swallowing the rest.
        for bad in ["var v = m[key", "var v = m[]", "m[key] ="] {
            let out = parse_text(bad);
            assert!(!out.diagnostics.is_empty(), "{bad} must report");
        }
    }

    /// **A type constructor's name followed by `[` opens a type-argument list;
    /// every other name followed by `[` is a subscript.**
    ///
    /// The two forms are the same two characters, and their contents cannot
    /// break the tie either (`Int` is a legal expression, `(Int, Int)` a legal
    /// tuple), so the name in front is the whole rule and this is where it is
    /// pinned.
    #[test]
    fn a_type_constructors_brackets_are_type_arguments_and_every_other_names_are_a_subscript() {
        let count = |text: &str, kind: SyntaxKind| -> usize {
            let out = parse_text(text);
            assert!(out.diagnostics.is_empty(), "{text}: {:?}", out.diagnostics);
            construct_names(&out.tree)
                .into_iter()
                .filter(|k| *k == kind)
                .count()
        };

        // §3.3's own spelling, and the shapes around it.
        for src in [
            "var c = Counter[(Int, Int)]()",
            "var v = Vec[Int]()",
            "var m = Map[Text, Int]()",
            "var g = Grid[Vec[Int]]()",
            // A trailing comma closes this list too.
            "var m = Map[Text, Int,]()",
        ] {
            assert_eq!(count(src, SyntaxKind::TYPE_ARG_LIST), 1, "{src}");
            assert_eq!(count(src, SyntaxKind::INDEX_EXPR), 0, "{src}");
            assert_eq!(count(src, SyntaxKind::CALL_EXPR), 1, "{src}");
        }

        // …and every other name's brackets are still a subscript, including a
        // subscript **followed by a call**, which is what a "brackets before `(`
        // are type arguments" rule would have broken.
        for src in [
            "var v = m[key]",
            "var v = m[key](7)",
            "var v = counter[key]",
            "var v = grid[x, y]",
        ] {
            assert_eq!(count(src, SyntaxKind::INDEX_EXPR), 1, "{src}");
            assert_eq!(count(src, SyntaxKind::TYPE_ARG_LIST), 0, "{src}");
        }

        // A type-argument list belongs to a *call*, so a bare one reports rather
        // than parsing as a type in value position. An empty one reports too: a
        // constructor with no arguments is spelled `Counter()`.
        for bad in [
            "var c = Counter[Int]",
            "var c = Counter[]()",
            "var c = Counter[Int",
            "var c = Vec[Int] + 1",
        ] {
            let out = parse_text(bad);
            assert!(!out.diagnostics.is_empty(), "{bad} must report");
        }
    }

    /// A `[` that **begins** an expression opens a list literal; a `[` that
    /// continues one is still a subscript.
    ///
    /// The two spellings are the same two characters, and — as with a
    /// type-argument list and a `(` — their contents cannot break the tie: `[k]`
    /// is a legal list and a legal subscript. Position is the whole rule, and
    /// this is where it is pinned.
    #[test]
    fn a_bracket_that_begins_an_expression_is_a_list_and_one_that_continues_it_is_a_subscript() {
        let count = |src: &str, kind: SyntaxKind| -> usize {
            let out = parse_text(src);
            assert!(out.diagnostics.is_empty(), "{src}: {:?}", out.diagnostics);
            construct_names(&out.tree)
                .into_iter()
                .filter(|k| *k == kind)
                .count()
        };

        // Every position an expression can begin in: a binding, an argument, a
        // `for` iterable, a return value, an element of another list.
        for src in [
            "var v = [1, 2, 3]",
            "var v = []",
            "var v = [1]",
            // A trailing comma closes this list too.
            "var v = [1, 2,]",
            "out([1, 2])",
            "for x in [1, 2] { out(x) }",
            "fn f() { return [1] }",
        ] {
            assert!(count(src, SyntaxKind::LIST_EXPR) >= 1, "{src}");
            assert_eq!(count(src, SyntaxKind::INDEX_EXPR), 0, "{src}");
        }

        // One node kind at every arity, including zero: nothing about a list
        // changes at two the way a paren becomes a tuple there.
        for (src, want) in [
            ("var v = []", 1),
            ("var v = [1]", 1),
            ("var v = [1, 2]", 1),
            ("var v = [[1], [2, 3]]", 3),
        ] {
            assert_eq!(count(src, SyntaxKind::LIST_EXPR), want, "{src}");
        }

        // …and a `[` that continues an expression is a subscript, including one
        // that indexes a list literal.
        for (src, want) in [
            ("var v = m[key]", 1),
            ("var v = grid[x, y]", 1),
            // Chained: each link continues the whole expression before it.
            ("var v = m[k][j]", 2),
            // A list literal is itself something a subscript can continue.
            ("var v = [1, 2][0]", 1),
            ("var v = f()[0]", 1),
        ] {
            assert_eq!(count(src, SyntaxKind::INDEX_EXPR), want, "{src}");
        }
        assert_eq!(count("var v = [1, 2][0]", SyntaxKind::LIST_EXPR), 1);

        // An empty subscript is an error: a subscript selects *something*, where
        // a list may hold nothing.
        for bad in ["var v = m[]", "var v = [1, 2", "var v = [1 2]"] {
            let out = parse_text(bad);
            assert!(!out.diagnostics.is_empty(), "{bad} must report");
        }
    }

    /// **A `for` binding is a pattern**, so `for (k, v) in m` takes the pair
    /// apart where `for kv in m` could only name it. Destructuring in binding
    /// position *is* a pattern, so the header reuses the pattern grammar rather
    /// than having one of its own (ADR-066 decision 3).
    #[test]
    fn a_for_binding_is_a_pattern() {
        let count = |src: &str, kind: SyntaxKind| -> usize {
            let out = parse_text(src);
            assert!(out.diagnostics.is_empty(), "{src}: {:?}", out.diagnostics);
            construct_names(&out.tree)
                .into_iter()
                .filter(|k| *k == kind)
                .count()
        };

        // The shapes, and the count of patterns each holds: the header's own,
        // plus one per element or field.
        for (src, patterns) in [
            ("for x in v { }", 1),
            ("for (k, v) in m { }", 3),
            ("for (a, (b, c)) in v { }", 5),
            ("for P { x, y } in ps { }", 1),
            ("for P { at: (x, y) } in ps { }", 4),
            ("for _ in v { }", 1),
        ] {
            assert_eq!(count(src, SyntaxKind::PATTERN), patterns, "{src}");
            assert_eq!(count(src, SyntaxKind::FOR_EXPR), 1, "{src}");
        }

        // The pattern is followed by `in`, never by `{`, so a record pattern's
        // brace cannot be read as the loop body — and the iterator keeps its own
        // record-literal suppression.
        let out = parse_text("for P { x } in near(Origin { x: 0 }) { 0 }");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);

        // A missing `in`, and a missing pattern, both still report.
        for bad in ["for x v { }", "for in v { }", "for (x, in v { }"] {
            let out = parse_text(bad);
            assert!(!out.diagnostics.is_empty(), "{bad} must report");
        }
    }

    /// **`min=` and `max=` are operators exactly where an identifier cannot be
    /// one**, and `min` is a name everywhere else.
    ///
    /// §6.2 writes `distance[key] min= candidate` and `best[key] max= score`.
    /// `min` is an `Ident`, so `min=` is two tokens the parser joins by context:
    /// a lexer rule that claimed `min=` would take `min` away from every program
    /// that calls the prelude helper (ADR-058), which §3.3's own program does.
    #[test]
    fn an_updating_store_is_an_operator_only_where_a_name_cannot_be() {
        let count = |src: &str, kind: SyntaxKind| -> usize {
            let out = parse_text(src);
            assert!(out.diagnostics.is_empty(), "{src}: {:?}", out.diagnostics);
            construct_names(&out.tree)
                .into_iter()
                .filter(|k| *k == kind)
                .count()
        };

        // §6.2's own two lines, and the shapes around them: a computed index, a
        // computed value, and the operator inside a block.
        for src in [
            "distance[key] min= candidate",
            "best[key] max= score",
            "d[a + b] min= f(x)",
            "fn go() {\n  d[k] max= n\n}",
            "grid[x, y] min= 3",
        ] {
            assert_eq!(count(src, SyntaxKind::UPDATE_OP), 1, "{src}");
            assert_eq!(count(src, SyntaxKind::PLACE_ASSIGN_STMT), 1, "{src}");
        }

        // The operator is **adjacent**, as `+=` is: with a space it is an
        // identifier followed by `=`, which is two statements run together and
        // reports as one.
        for spaced in ["d[k] min = 3", "d[k] max = 3"] {
            let out = parse_text(spaced);
            assert!(!out.diagnostics.is_empty(), "{spaced} must report");
        }

        // `min` and `max` are ordinary names everywhere else — the whole reason
        // the rule is contextual.
        for src in [
            "var d = min(3, 4)",
            "var d = max(abs(a), abs(b))",
            "var m = min",
            "out(min(1, 2) + max(3, 4))",
            // …including as the receiver of a subscript, where the identifier is
            // followed by `[` and not by `=`.
            "var v = min[0]",
        ] {
            let out = parse_text(src);
            assert!(out.diagnostics.is_empty(), "{src}: {:?}", out.diagnostics);
            assert_eq!(count(src, SyntaxKind::UPDATE_OP), 0, "{src}");
        }

        // `==` is one token by max-munch, so a comparison can never be read as an
        // update, and no other identifier gets the rule.
        for src in ["var r = d[k] == v", "var r = m[k] == 3"] {
            assert_eq!(count(src, SyntaxKind::UPDATE_OP), 0, "{src}");
        }
        let out = parse_text("d[k] mid= 3");
        assert!(
            !out.diagnostics.is_empty(),
            "only `min` and `max` are operators"
        );

        // A target that is not a place still *parses*, exactly as `f() = 1`
        // does: naming no storage is inference's report and not the parser's.
        for src in ["x min= 1", "f() max= 1"] {
            let out = parse_text(src);
            assert!(out.diagnostics.is_empty(), "{src}: {:?}", out.diagnostics);
            assert_eq!(count(src, SyntaxKind::UPDATE_OP), 1, "{src}");
        }

        // …and a missing value is a mistake, not an empty store.
        let out = parse_text("d[k] min=");
        assert!(!out.diagnostics.is_empty(), "a value is required");
    }

    /// §9.8's `:bp` marker rides the same rule an updating store does, at the
    /// one other position where an identifier can decide an operator: the end of
    /// a statement, where a `:` begins nothing else.
    #[test]
    fn a_breakpoint_marker_is_a_marker_only_where_a_type_cannot_be() {
        let count = |src: &str, kind: SyntaxKind| -> usize {
            let out = parse_text(src);
            assert!(out.diagnostics.is_empty(), "{src}: {:?}", out.diagnostics);
            construct_names(&out.tree)
                .into_iter()
                .filter(|k| *k == kind)
                .count()
        };

        // Every statement form takes one, and it lands inside that statement's
        // own node rather than beside it.
        for (src, stmt) in [
            ("var a = 1 :bp", SyntaxKind::VAR_STMT),
            ("var a: Int = 1 :bp", SyntaxKind::VAR_STMT),
            (
                "fn f() {\n  var a = 1\n  a = 2 :bp\n}",
                SyntaxKind::ASSIGN_STMT,
            ),
            ("var m = [1]\nm[0] = 2 :bp", SyntaxKind::PLACE_ASSIGN_STMT),
            (
                "var m = [1]\nm[0] min= 2 :bp",
                SyntaxKind::PLACE_ASSIGN_STMT,
            ),
            ("out(1) :bp", SyntaxKind::EXPR_STMT),
            ("{ }\n:bp", SyntaxKind::EXPR_STMT),
        ] {
            assert_eq!(count(src, SyntaxKind::BREAKPOINT), 1, "{src}");
            assert_eq!(count(src, stmt), 1, "{src}");
        }

        // The marker is a *child of the statement*, which is what lets lowering
        // ask a statement node whether it carries one.
        let out = parse_text("var a = 1 :bp");
        let var_stmt = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::VAR_STMT)
            .expect("a var statement");
        assert!(
            var_stmt
                .children()
                .any(|c| c.kind() == SyntaxKind::BREAKPOINT),
            "the marker is the statement's child"
        );

        // Adjacent, exactly as `min=` is: with a space it is a `:` that begins
        // nothing, and the statement runs on.
        for spaced in ["var a = 1 : bp", "out(1) : bp"] {
            let out = parse_text(spaced);
            assert!(!out.diagnostics.is_empty(), "{spaced} must report");
        }

        // `bp` is an ordinary name everywhere else — the whole reason the rule
        // is contextual — including as a binding, a call and a type annotation's
        // *value* position.
        for src in [
            "var bp = 1",
            "var a = bp",
            "fn bp() {\n  out(1)\n}",
            "var a: Int = 1\nvar bp = a",
            // The other `:`s in the grammar are inside declarations, where a
            // statement has not ended and this rule is never asked.
            "struct P { bp: Int }",
            "fn f(bp: Int) -> Int {\n  bp\n}",
        ] {
            let out = parse_text(src);
            assert!(out.diagnostics.is_empty(), "{src}: {:?}", out.diagnostics);
            assert_eq!(count(src, SyntaxKind::BREAKPOINT), 0, "{src}");
        }

        // A marker on a nested statement belongs to *that* statement, not to the
        // block that contains it: the outer `EXPR_STMT` has no marker child.
        let out = parse_text("{\n  out(1) :bp\n}");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let outer = out
            .tree
            .children()
            .find(|n| n.kind() == SyntaxKind::EXPR_STMT)
            .expect("the block statement");
        assert!(
            !outer.children().any(|c| c.kind() == SyntaxKind::BREAKPOINT),
            "the inner statement's marker is not the outer statement's"
        );
    }

    /// **A declaration's members are separated by a comma or a line break** —
    /// which is how §4.5 and §4.6 write their own. Match arms take either
    /// separator (D8, ADR-049); struct fields and enum variants are the same
    /// rule at the same kind of brace.
    #[test]
    fn a_declarations_members_take_a_comma_or_a_line_break() {
        // The design doc's own text, verbatim, and the comma form beside it: the
        // same tree once the comma token is out of the way, which is what "either
        // separator" means.
        for (breaks, commas) in [
            (
                "struct Point {\n    x: Int\n    y: Int\n}",
                "struct Point { x: Int, y: Int }",
            ),
            (
                "enum Tile {\n    Empty\n    Wall\n    Number(Int)\n    Portal(Text)\n}",
                "enum Tile { Empty, Wall, Number(Int), Portal(Text) }",
            ),
            // Mixed, in both orders — the two separators are interchangeable and
            // not two dialects.
            (
                "struct P {\n    x: Int, y: Int\n    z: Int\n}",
                "struct P { x: Int, y: Int, z: Int }",
            ),
            ("enum E {\n    A, B\n    C\n}", "enum E { A, B, C }"),
            // …and a trailing comma still closes the list, whichever preceded it.
            (
                "struct P {\n    x: Int\n    y: Int,\n}",
                "struct P { x: Int, y: Int }",
            ),
            ("enum E {\n    A\n    B,\n}", "enum E { A, B }"),
        ] {
            let out = parse_text(breaks);
            assert!(
                out.diagnostics.is_empty(),
                "{breaks}: {:?}",
                out.diagnostics
            );
            let filt = |t: &str| -> Vec<SyntaxKind> {
                construct_names(&parse_text(t).tree)
                    .into_iter()
                    .filter(|k| !k.is_trivia() && *k != SyntaxKind::COMMA)
                    .collect()
            };
            assert_eq!(filt(breaks), filt(commas), "{breaks}");
        }

        // The rule is a separator, not "separators are optional": two members on
        // one line with neither is still a mistake.
        for bad in [
            "struct P { x: Int y: Int }",
            "enum E { A B }",
            "enum E { A(Int) B }",
        ] {
            let out = parse_text(bad);
            assert!(!out.diagnostics.is_empty(), "{bad} must report");
        }

        // The shapes with no separator to give: an empty declaration and a
        // one-member one, on one line and across lines.
        for src in [
            "struct P { }",
            "enum E { }",
            "struct P { x: Int }",
            "struct P {\n    x: Int\n}",
            "enum E {\n    A\n}",
        ] {
            let out = parse_text(src);
            assert!(out.diagnostics.is_empty(), "{src}: {:?}", out.diagnostics);
        }
    }

    /// **A record pattern and a tuple pattern are patterns wherever a pattern is
    /// legal**, and each carries its sub-patterns in a shape the rest of the
    /// compiler can read.
    ///
    /// The list of shapes matters more than any one of them. A record pattern's
    /// fields are `PATTERN_FIELD`s and a tuple's elements are bare `PATTERN`s, so
    /// `P { x }` never looks like `P(x)`, and `P { x: p }` and `P { x }` are one
    /// node shape with an optional child rather than two identifier-counting
    /// rules.
    #[test]
    fn a_record_pattern_names_fields_and_a_tuple_pattern_names_positions() {
        let count = |src: &str, kind: SyntaxKind| -> usize {
            let out = parse_text(src);
            assert!(out.diagnostics.is_empty(), "{src}: {:?}", out.diagnostics);
            construct_names(&out.tree)
                .into_iter()
                .filter(|k| *k == kind)
                .count()
        };

        // A record pattern's fields, punned and explicit and mixed. The `{` is
        // unambiguous here where a record *literal*'s is not: a pattern is
        // followed by `=>`, never by a block.
        for (src, fields) in [
            ("var a = match p { P { x } => x }", 1),
            ("var a = match p { P { x, y } => x }", 2),
            ("var a = match p { P { x: 1, y } => y }", 2),
            ("var a = match p { P { x: q, y: r } => q }", 2),
            // A trailing comma closes this list too.
            ("var a = match p { P { x, y, } => x }", 2),
        ] {
            assert_eq!(count(src, SyntaxKind::PATTERN_FIELD), fields, "{src}");
        }

        // A tuple pattern's elements are sub-patterns, at every arity and nested.
        // The counts include the arm's own outer pattern.
        for (src, patterns) in [
            ("var a = match t { (x, y) => x }", 3),
            ("var a = match t { (x, y, z) => x }", 4),
            ("var a = match t { (x, (y, z)) => x }", 5),
            ("var a = match t { (1, _) => 0, _ => 1 }", 4),
            // …and a trailing comma.
            ("var a = match t { (x, y,) => x }", 3),
        ] {
            assert_eq!(count(src, SyntaxKind::PATTERN), patterns, "{src}");
        }

        // The two compose: a record field holding a tuple, a tuple element
        // holding a record, and a variant payload holding either.
        for src in [
            "var a = match p { P { at: (x, y) } => x }",
            "var a = match t { (P { x }, n) => x }",
            "var a = match o { Some(P { x, y }) => x, None => 0 }",
            "var a = match o { Some((x, y)) => x, None => 0 }",
        ] {
            let out = parse_text(src);
            assert!(out.diagnostics.is_empty(), "{src}: {:?}", out.diagnostics);
        }

        // A tuple pattern must be able to *start* an arm, or the arm list stops
        // at it and every arm after it silently leaves the tree — which is what
        // `is_pattern_start` decides.
        let out = parse_text("var a = match t { (x, y) => x\n _ => 0 }");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert_eq!(
            count(
                "var a = match t { (x, y) => x\n _ => 0 }",
                SyntaxKind::MATCH_ARM
            ),
            2,
            "both arms are in the tree"
        );

        // The shapes that are not patterns. `()` has no type to match — `Unit` is
        // not a tuple — and a field with a `:` and nothing after it is a pattern
        // the program did not finish writing.
        for bad in [
            "var a = match u { () => 0 }",
            "var a = match p { P { x: } => 0 }",
            "var a = match p { P { , x } => 0 }",
            "var a = match p { P { x => 0 }",
            "var a = match t { (x, => 0 }",
        ] {
            let out = parse_text(bad);
            assert!(!out.diagnostics.is_empty(), "{bad} must report");
        }
    }

    /// **A record pattern's head is optional** (ADR-091). It is the headed
    /// production with the head made optional, so it costs one arm in
    /// `parse_pattern`, one entry in `is_pattern_start`, and no new token.
    #[test]
    fn a_record_pattern_needs_no_head() {
        let count = |src: &str, kind: SyntaxKind| -> usize {
            let out = parse_text(src);
            assert!(out.diagnostics.is_empty(), "{src}: {:?}", out.diagnostics);
            construct_names(&out.tree)
                .into_iter()
                .filter(|k| *k == kind)
                .count()
        };

        // The fields are the headed form's, unchanged — punned, explicit, mixed,
        // and with a trailing comma.
        for (src, fields) in [
            ("var a = match p { {x} => x }", 1),
            ("var a = match p { {x, y} => x }", 2),
            ("var a = match p { {x: 1, y} => y }", 2),
            ("var a = match p { {x: q, y: r} => q }", 2),
            ("var a = match p { {x, y,} => x }", 2),
        ] {
            assert_eq!(count(src, SyntaxKind::PATTERN_FIELD), fields, "{src}");
        }

        // One production, so it composes in every position a pattern appears:
        // nested in a variant's payload (the shape a `choice(...)` payload record
        // needs), in a tuple, in a `for` header, and as a closure parameter.
        for src in [
            "var a = match m { Mul({x, y}) => x, Do(_) => 0 }",
            "var a = match t { ({x}, n) => x }",
            "var a = match p { {at: (x, y)} => x }",
            "for {x, y} in ps { out(x) }",
            "var f = |{x, y}| x + y",
        ] {
            let out = parse_text(src);
            assert!(out.diagnostics.is_empty(), "{src}: {:?}", out.diagnostics);
        }

        // A headless pattern must be able to *start* an arm, or the arm list
        // stops before it and it — with every arm after it — silently leaves the
        // tree. The headless arm is written **second** on purpose: the first
        // arm's pattern is parsed unconditionally, so only a later one exercises
        // `is_pattern_start`.
        assert_eq!(
            count(
                "var a = match p { _ => 0\n {x, y} => x }",
                SyntaxKind::MATCH_ARM
            ),
            2,
            "both arms are in the tree"
        );

        // `{}` is rejected where `()` is, and for the same reason (ADR-091
        // Decision 3): it binds nothing and names no record, so it is an
        // irrefutable arm written by accident. The pattern that matches
        // everything is spelled `_`.
        let out = parse_text("var a = match p { {} => 0 }");
        assert!(
            out.diagnostics
                .iter()
                .any(|d| d.message().contains("expected a pattern")),
            "an empty headless record pattern must report: {:?}",
            out.diagnostics
        );

        // …but a *headed* `P {}` is kept: it names the record it tests for, so it
        // is refutable — `Some` beside `Some(_)`.
        let out = parse_text("var a = match p { P {} => 0 }");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
    }

    /// **A `(` that begins a line begins something new**; a `(` on the same line
    /// as the expression before it is that expression's argument list.
    ///
    /// `peek()` skips trivia and a newline **is** trivia, so without the rule the
    /// postfix loop and `parse_name_or_call` would open a `CALL_EXPR` on a
    /// line-leading `(`. In a `match` that is silent data loss: the arm body
    /// swallows the next arm's tuple pattern as an argument list, the arm loop
    /// finds no pattern start, and every arm after the first leaves the tree.
    #[test]
    fn a_line_leading_paren_begins_a_new_thing_and_a_same_line_one_is_a_call() {
        let count = |src: &str, kind: SyntaxKind| -> usize {
            let out = parse_text(src);
            assert!(out.diagnostics.is_empty(), "{src}: {:?}", out.diagnostics);
            construct_names(&out.tree)
                .into_iter()
                .filter(|k| *k == kind)
                .count()
        };

        // Three arms, and all three are in the tree. A `10(a, b)` call would
        // leave one arm and one `CALL_EXPR`.
        let arms = "var r = match p {\n    (0, 0) => 10\n    (a, b) => a + b\n    _ => 0\n}";
        assert_eq!(count(arms, SyntaxKind::MATCH_ARM), 3);
        assert_eq!(count(arms, SyntaxKind::CALL_EXPR), 0);

        // The same shape after every kind of arm body a `(` could attach itself
        // to — a name, a call, a subscript, a field read, a block.
        for body in ["n", "f(1)", "m[k]", "p.x", "{ 0 }"] {
            let src = format!("var r = match p {{\n    _ => {body}\n    (a, b) => 1\n}}");
            assert_eq!(count(&src, SyntaxKind::MATCH_ARM), 2, "{src}");
        }

        // The other direction, which is the rule's whole content: on one line a
        // `(` still opens an argument list, through every callee shape — a name,
        // a call's result, a subscript's, a paren, a closure.
        for src in [
            "var a = f(1)",
            "var a = f(1)(2)",
            "var a = m[k](7)",
            "var a = (g)(3)",
            "var a = (|x| x * 3)(14)",
            "var a = fs.get(0)(100)",
        ] {
            assert!(count(src, SyntaxKind::CALL_EXPR) >= 1, "{src}");
        }

        // A `[` is subject to the same rule, because a list literal begins with
        // one: `m\n[k]` is a binding and a list, not a subscript.
        let sub = "var a = m\n[k]";
        assert_eq!(count(sub, SyntaxKind::INDEX_EXPR), 0);
        assert_eq!(count(sub, SyntaxKind::LIST_EXPR), 1);
        assert_eq!(count(sub, SyntaxKind::VAR_STMT), 1);
        // On one line it is a subscript.
        assert_eq!(count("var a = m[k]", SyntaxKind::INDEX_EXPR), 1);
        assert_eq!(count("var a = m[k]", SyntaxKind::LIST_EXPR), 0);

        // Nor is the Pratt loop (ADR-049 D8): an operator that ends a line still
        // continues across it, and so does a `.method()` chain.
        assert_eq!(count("var a = 1 +\n2", SyntaxKind::BIN_EXPR), 1);
        assert_eq!(
            count("var a = v\n  .len()", SyntaxKind::METHOD_CALL_EXPR),
            1
        );
        assert_eq!(
            count(
                "var a = v\n  .map(f)\n  .sum()",
                SyntaxKind::METHOD_CALL_EXPR
            ),
            2
        );

        // A `(` that *opens* an expression is untouched wherever it appears —
        // only a `(` asked to continue one is.
        assert_eq!(count("var a = 1\n(b, c)", SyntaxKind::TUPLE_EXPR), 1);
        assert_eq!(count("var a = 1\n(b, c)", SyntaxKind::VAR_STMT), 1);
        assert_eq!(count("var a = 1\n(b + c) * 2", SyntaxKind::PAREN_EXPR), 1);

        // …and a `for` binding is the second place a tuple pattern makes a
        // line-leading `(` reachable.
        assert_eq!(
            count("var a = 1\nfor (k, v) in m { }", SyntaxKind::FOR_EXPR),
            1
        );

        // The cost, stated as a test rather than left to be discovered: a callee
        // that ends a line and an argument list that begins the next are two
        // expressions, so this is two statements and not one call.
        let split = "var a = f\n(1)";
        assert_eq!(count(split, SyntaxKind::CALL_EXPR), 0);
        assert_eq!(count(split, SyntaxKind::PAREN_EXPR), 1);
    }

    /// **`||` is an empty parameter list where an expression must begin, and
    /// logical-or everywhere else.** The lexer's max-munch makes it one token,
    /// so position is the only thing that can tell the two apart — §4.2's
    /// shadowing example is `var show_old = || out(a)`.
    #[test]
    fn a_double_pipe_is_a_closure_where_an_expression_begins_and_an_operator_between_two() {
        let count = |src: &str, kind: SyntaxKind| -> usize {
            let out = parse_text(src);
            assert!(out.diagnostics.is_empty(), "{src}: {:?}", out.diagnostics);
            construct_names(&out.tree)
                .into_iter()
                .filter(|k| *k == kind)
                .count()
        };

        // §4.2's own line, and the zero-parameter closure in every position an
        // expression begins: a binding, an argument, a block tail, a `return`, an
        // operand of the very operator it is spelled like, and its own body.
        for src in [
            "var show_old = || out(a)",
            "var f = || 5",
            "out(|| 5)",
            "fn g() { || 5 }",
            "fn g() { return || 5 }",
            "var f = a || || 5",
            "var f = || || 7",
            "var f = if p { || 1 } else { || 2 }",
        ] {
            assert!(count(src, SyntaxKind::CLOSURE_EXPR) >= 1, "{src}");
            assert_eq!(count(src, SyntaxKind::PARAM), 0, "{src}");
        }

        // `| |` with a space is the same closure — the two spellings differ only
        // in how the lexer munched them.
        assert_eq!(count("var f = | | 5", SyntaxKind::CLOSURE_EXPR), 1);

        // The other direction: between two operands `||` is logical-or, whose
        // precedence is the lowest of all — below `..` and `&&`.
        assert_eq!(count("var a = p || q", SyntaxKind::BIN_EXPR), 1);
        assert_eq!(count("var a = p || q", SyntaxKind::CLOSURE_EXPR), 0);
        assert_eq!(
            shape("var a = p || q && r"),
            shape("var a = p || (q && r)"),
            "`&&` still binds tighter than `||`"
        );
        assert_eq!(
            shape("var a = p == q || r == s"),
            shape("var a = (p == q) || (r == s)"),
            "comparison still binds tighter than `||`"
        );
        assert_eq!(
            shape("var a = p || q || r"),
            shape("var a = (p || q) || r"),
            "`||` is still left-associative"
        );

        // A one-parameter closure is untouched, which is what says the
        // zero-parameter arm only fires on the two-pipe token.
        assert_eq!(count("var f = |x| x", SyntaxKind::PARAM), 1);
    }

    /// **A closure parameter is a pattern, not a bare name** — Appendix D writes
    /// `|(a, b)| abs(a - b)`. It is the same grammar the `for` binding uses, at
    /// the third and last binding position.
    #[test]
    fn a_closure_parameter_is_a_pattern() {
        let count = |src: &str, kind: SyntaxKind| -> usize {
            let out = parse_text(src);
            assert!(out.diagnostics.is_empty(), "{src}: {:?}", out.diagnostics);
            construct_names(&out.tree)
                .into_iter()
                .filter(|k| *k == kind)
                .count()
        };

        // Appendix D's own line.
        assert_eq!(
            count(
                "var d = left.zip(right).map(|(a, b)| abs(a - b)).sum()",
                SyntaxKind::CLOSURE_EXPR
            ),
            1
        );

        // The shapes, and the pattern count each holds: the parameter's own, plus
        // one per element or nested field.
        for (src, params, patterns) in [
            ("var f = |x| x", 1, 1),
            ("var f = |_| 0", 1, 1),
            ("var f = |(a, b)| a", 1, 3),
            ("var f = |(a, (b, c))| a", 1, 5),
            ("var f = |P { x, y }| x", 1, 1),
            ("var f = |P { at: (x, y) }| x", 1, 4),
            ("var f = |(a, b), c| a", 2, 4),
            ("var f = |a, (b, c)| a", 2, 4),
            ("var f = | | 0", 0, 0),
        ] {
            assert_eq!(count(src, SyntaxKind::PARAM), params, "{src}");
            assert_eq!(count(src, SyntaxKind::PATTERN), patterns, "{src}");
            assert_eq!(count(src, SyntaxKind::CLOSURE_EXPR), 1, "{src}");
        }

        // A pattern parameter still takes an annotation, and the annotation is the
        // whole argument's — the `:` is what ends the pattern.
        assert_eq!(
            count("var f = |(a, b): (Int, Int)| a", SyntaxKind::TUPLE_TYPE),
            1
        );
        assert_eq!(count("var f = |x: Int| x", SyntaxKind::TYPE_REF), 1);

        // A trailing comma still closes the list, and a record pattern's brace
        // is not read as anything else: a parameter is followed by `,`, `:` or
        // `|`, never by an expression.
        for src in ["var f = |(a, b),| a", "var f = |P { x }| P { x: x }"] {
            assert_eq!(count(src, SyntaxKind::CLOSURE_EXPR), 1, "{src}");
        }

        // The malformed shapes still report.
        for bad in [
            "var f = |(a, | a",
            "var f = |(| a",
            "var f = |+| a",
            "var f = |a, | ",
        ] {
            let out = parse_text(bad);
            assert!(!out.diagnostics.is_empty(), "{bad} must report");
        }
    }

    /// The construct shape of `text` with parentheses erased, so two spellings
    /// that differ only by explicit grouping compare equal exactly when they
    /// parse to the same tree. Comparing the raw kind lists could not: the
    /// parenthesized form has `PAREN_EXPR`, `L_PAREN` and `R_PAREN` in it.
    fn shape(text: &str) -> Vec<SyntaxKind> {
        let out = parse_text(text);
        assert!(out.diagnostics.is_empty(), "{text}: {:?}", out.diagnostics);
        construct_names(&out.tree)
            .into_iter()
            .filter(|k| {
                !k.is_trivia()
                    && !matches!(
                        k,
                        SyntaxKind::PAREN_EXPR | SyntaxKind::L_PAREN | SyntaxKind::R_PAREN
                    )
            })
            .collect()
    }

    fn construct_names(node: &SyntaxNode) -> Vec<SyntaxKind> {
        let mut out = Vec::new();
        collect(node, &mut out);
        return out;
        fn collect(node: &SyntaxNode, out: &mut Vec<SyntaxKind>) {
            out.push(node.kind());
            for child in node.children_with_tokens() {
                match child {
                    rowan::NodeOrToken::Node(n) => collect(&n, out),
                    rowan::NodeOrToken::Token(t) => out.push(t.kind()),
                }
            }
        }
    }
}
