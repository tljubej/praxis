//! The Praxis parser (Milestone 1 subset).
//!
//! Recursive descent over statements and structure, with a Pratt (precedence
//! climbing) loop for arithmetic and the other binary operators (ADR-004). It
//! consumes the lexer's [`Token`] stream and emits a rowan green tree
//! (ADR-003) via [`GreenNodeBuilder`], retaining trivia so the tree is lossless
//! (§13.1). On an unexpected token it emits a `P0xx` diagnostic, wraps the stray
//! token in a [`SyntaxKind::PARSE_ERROR`] node, advances to a synchronization
//! point, and keeps going — that is the LSP-grade recovery required by §15.2.
//!
//! The M1 grammar (§19) covers: literals, `let`/`var` bindings, blocks, calls
//! (including `out(...)`), `fn` items, arithmetic, `if`/`else`, and `while`.
//! Other constructs are not parsed; they recover with a diagnostic.

use praxis_source::{BytePos, Span};
use praxis_source::{Diagnostic, DiagnosticCategory, DiagnosticCode, FileId, FileSpan, Severity};
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
    diagnostics.sort_by_key(|d| {
        let span = d.primary().span;
        (span.start(), span.end())
    });
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
/// ones would call `expr(min_bp)` (none exist in the M1 set, but the table is
/// shaped to allow it).
#[derive(Clone, Copy)]
struct BindingPower {
    left: u8,
    right: u8,
}

/// The binding power of `op`, or `None` if it is not a binary operator.
fn infix_binding_power(op: SyntaxKind) -> Option<BindingPower> {
    Some(match op {
        // Logical (lowest) — `||` not in the M1 eval set but lexed, so bind it.
        SyntaxKind::PIPE2 => bp(1, 2),
        // Comparison (non-associative in spirit; we parse left-assoc).
        SyntaxKind::EQ2
        | SyntaxKind::NEQ
        | SyntaxKind::LT
        | SyntaxKind::GT
        | SyntaxKind::LTEQ
        | SyntaxKind::GTEQ => bp(3, 4),
        // Additive.
        SyntaxKind::PLUS | SyntaxKind::MINUS => bp(5, 6),
        // Multiplicative.
        SyntaxKind::STAR | SyntaxKind::SLASH | SyntaxKind::PERCENT => bp(7, 8),
        _ => return None,
    })
}

/// Prefix (unary) operator binding power.
fn prefix_binding_power(op: SyntaxKind) -> Option<u8> {
    match op {
        SyntaxKind::MINUS | SyntaxKind::BANG => Some(9),
        // `read` is a prefix expression (§7.1): `read parser_expression`. Its
        // body is a parser-expression, not an ordinary expression, so it gets
        // the highest binding power (binds tighter than arithmetic).
        SyntaxKind::KW_READ => Some(11),
        _ => None,
    }
}

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

/// Whether `kind` can start a match pattern (M7, §4.6). Used to decide whether
/// to continue parsing arms after a newline (arms are comma-OR-newline separated).
fn is_pattern_start(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::UNDERSCORE
            | SyntaxKind::IntLit
            | SyntaxKind::TextLit
            | SyntaxKind::KW_TRUE
            | SyntaxKind::KW_FALSE
            | SyntaxKind::Ident
    )
}

const fn bp(left: u8, right: u8) -> BindingPower {
    BindingPower { left, right }
}

// ---------------------------------------------------------------------------
// Statement separation (F8 / FE-04; D8, ADR-049).
// ---------------------------------------------------------------------------

/// `P001` — the general "this token cannot go here" parse error.
const UNEXPECTED: DiagnosticCode = DiagnosticCode::new(DiagnosticCategory::Parse, 1);

/// `P002` — two statements ran together with nothing between them (FE-04).
const MISSING_SEPARATOR: DiagnosticCode = DiagnosticCode::new(DiagnosticCategory::Parse, 2);

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
    /// When true, `Name { … }` is *not* parsed as a record literal. This is set
    /// while parsing `if`/`while` conditions so the `{ then-block }` is not
    /// consumed as a record body. Block-expression contexts (`({ … })`) are
    /// unaffected because the block follows `(`, not a bare name.
    no_struct_literal: bool,
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
            no_struct_literal: false,
        }
    }

    // --- entry point ---

    fn parse_source_file(&mut self) {
        self.start_node(SyntaxKind::SOURCE_FILE);
        while !self.at_end() {
            // Emit any trivia leading each statement into the root node.
            self.eat_trivia();
            if self.at_end() {
                break;
            }
            let before = self.meaningful_index();
            if self.parse_stmt() {
                // A statement is separated from the next by `;`, a newline, or
                // the end of the file — and by nothing else (FE-04).
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
        self.tokens[self.cursor..]
            .iter()
            .find(|token| !token.kind.is_trivia())
            .is_some_and(|token| token.preceded_by_newline)
    }

    /// True iff a value follows `break`/`return` on the same line.
    ///
    /// Two questions, both of which must say yes: the next token has to be able
    /// to begin an expression at all (a `;`, `}`, `)`, `,`, `else` or `in`
    /// cannot), and it has to be on *this* line — `return\n1` is a value-less
    /// return followed by a separate statement, which is the second half of
    /// D8's rule and the only place outside a statement loop that consults a
    /// newline.
    fn starts_expr(&mut self) -> bool {
        if self.newline_before() {
            return false;
        }
        // Consume trivia so the cursor lands on the meaningful token, then check
        // whether that token can begin an expression. `eat_trivia` returns ();
        // we only need its side effect of advancing past trivia.
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
            MISSING_SEPARATOR,
            span,
            "expected `;` or a line break between statements",
        );
        None
    }

    /// The source text of the current meaningful token (trivia skipped). Used to
    /// special-case keywords-that-look-like-idents such as `parse` and parser
    /// constructor names (`lines`, `csv`, …).
    fn peek_text(&mut self) -> Option<&str> {
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

    fn start_node(&mut self, kind: SyntaxKind) {
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
        self.error_with(UNEXPECTED, span, message);
    }

    fn error_with(&mut self, code: DiagnosticCode, span: Span, message: impl Into<String>) {
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
            SyntaxKind::KW_LET => self.parse_let_or_var(SyntaxKind::LET_STMT),
            SyntaxKind::KW_VAR => self.parse_let_or_var(SyntaxKind::VAR_STMT),
            SyntaxKind::KW_FN => self.parse_fn_item(),
            SyntaxKind::KW_STRUCT => self.parse_struct_item(),
            SyntaxKind::KW_ENUM => self.parse_enum_item(),
            // `name = expr` / `name += expr` reassignment (§4.2).
            SyntaxKind::Ident if is_assignment_op(self.nth_kind(1)) => self.parse_assign_stmt(),
            _ => self.parse_expr_stmt(),
        }
    }

    /// `let`/`var name [: Type] = expr` — `kind` is the node kind (LET_STMT / VAR_STMT).
    fn parse_let_or_var(&mut self, kind: SyntaxKind) -> bool {
        self.start_node(kind);
        self.bump(); // `let`/`var`
        self.expect_binder("binding name");
        // Optional type annotation `: Type` (M2: real type grammar — scalar,
        // tuple, or function type).
        if self.eat(SyntaxKind::COLON) {
            self.parse_type();
        }
        self.expect(SyntaxKind::EQ, "`=`");
        self.parse_expr();
        self.finish_node();
        true
    }

    /// `name = expr` or `name += expr` (etc.) — reassignment to an existing
    /// binding (§4.2). The lhs is a single name for M1; richer lvalues (fields,
    /// indexing) come with their constructs in later milestones.
    fn parse_assign_stmt(&mut self) -> bool {
        self.start_node(SyntaxKind::ASSIGN_STMT);
        self.bump(); // name
        self.bump(); // assignment operator (=, +=, ...)
        self.parse_expr();
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

    /// `struct Name { field: Type, … }` (M7, §4.5). The field list is a
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
                if !self.eat(SyntaxKind::COMMA) {
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

    /// `enum Name { Variant, Variant(Type, …), … }` (M7, §4.6). Each variant is
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
                            self.ensure_progress(pbefore);
                        }
                    }
                    self.expect(SyntaxKind::R_PAREN, "`)` to close variant payload");
                }
                self.finish_node(); // ENUM_VARIANT
                if !self.eat(SyntaxKind::COMMA) {
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
            self.finish_node();
            return true;
        }
        self.start_node(SyntaxKind::EXPR_STMT);
        self.parse_expr();
        self.finish_node();
        true
    }

    /// `{ stmt; stmt; expr }` — a block. The last expression is the block's
    /// value (§4.11). For M1 we treat every item as either a statement or a
    /// trailing expression.
    fn parse_block(&mut self) {
        self.start_node(SyntaxKind::BLOCK_EXPR);
        self.bump(); // `{`
        while !self.at(SyntaxKind::R_BRACE) && !self.at_end() {
            self.eat_trivia();
            if self.at(SyntaxKind::R_BRACE) || self.at_end() {
                break;
            }
            let before = self.meaningful_index();
            // Every item is a statement (let/var/fn/assignment) or a bare
            // expression statement (which may be the trailing expression). The
            // dispatch in parse_stmt covers all of these.
            self.parse_stmt();
            // A `;` is optional only because a newline separates just as well;
            // one of the two (or the closing `}`) has to be there (FE-04).
            self.expect_stmt_separator();
            // Guarantee termination on any input.
            self.ensure_progress(before);
        }
        self.expect(SyntaxKind::R_BRACE, "`}` to close block");
        self.finish_node();
    }

    // --- expressions (Pratt climbing) ---

    /// Entry into expression parsing at the lowest binding power.
    fn parse_expr(&mut self) {
        self.parse_expr_bp(0);
    }

    fn parse_expr_bp(&mut self, min_bp: u8) {
        // Capture the builder position *before* the left operand is emitted, so
        // a later infix operator can wrap (lhs op rhs) into a BIN_EXPR via
        // start_node_at. This is the standard rowan Pratt idiom.
        let cp = self.checkpoint_lhs();
        // Parse the left-hand side (prefix / atom).
        self.parse_prefix();

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
            // Wrap the already-emitted lhs + operator + rhs in a BIN_EXPR,
            // retroactively opening the node at the checkpoint taken before lhs.
            self.start_node_at(cp, SyntaxKind::BIN_EXPR);
            self.bump(); // operator
            self.parse_expr_bp(bp.right);
            self.finish_node();
        }
    }

    /// Prefix expression: unary operators, `read`, then an atom or a
    /// parenthesized expression. Followed by any postfix `expr(args)` calls
    /// (M8, §4.10) — calling a closure retrieved from a collection
    /// (`fs.get(0)(100)`), the result of another call (`f(1)(2)`), a paren
    /// (`(|x| x*3)(14)`), etc.
    fn parse_prefix(&mut self) {
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
        } else if op == SyntaxKind::PIPE {
            // `|params| expr` closure (M7, §4.10). Bare `PIPE` claims the `|`;
            // the lexer's max-munch keeps `||` as logical-or (`PIPE2`, handled
            // in the infix table), so the two never conflict.
            self.parse_closure();
        } else if let Some(bp) = prefix_binding_power(op) {
            self.start_node(SyntaxKind::UNARY_EXPR);
            self.bump(); // unary operator
            self.parse_expr_bp(bp);
            self.finish_node();
        } else {
            self.parse_atom();
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
                SyntaxKind::L_PAREN => {
                    self.start_node_at(cp, SyntaxKind::CALL_EXPR);
                    self.bump(); // `(`
                    self.parse_arg_list();
                    self.finish_node(); // CALL_EXPR
                }
                SyntaxKind::DOT => {
                    self.bump(); // `.`
                    if !self.at(SyntaxKind::Ident) {
                        let span = self.current_span();
                        self.error(span, "expected name after `.`");
                        break;
                    }
                    // Disambiguate field access (`p.x`) from method call
                    // (`p.x()`): an IDENT followed by `(` is a method call.
                    if self.nth_kind(1) == SyntaxKind::L_PAREN {
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
        self.start_node(SyntaxKind::ARG_LIST);
        if !self.at(SyntaxKind::R_PAREN) {
            loop {
                let before = self.meaningful_index();
                self.parse_expr();
                if !self.eat(SyntaxKind::COMMA) {
                    break;
                }
                // Guarantee termination on any input.
                self.ensure_progress(before);
            }
        }
        self.expect(SyntaxKind::R_PAREN, "`)`");
        self.finish_node(); // ARG_LIST
    }

    /// `|params| expr` — a closure expression (M7, §4.10). Params are bare names
    /// optionally annotated `: Type`, separated by commas, between two `|`. The
    /// body is a single expression (which may be a `{ block }`). Closures capture
    /// outer variables automatically (§4.10); the capture analysis is in HIR.
    fn parse_closure(&mut self) {
        self.start_node(SyntaxKind::CLOSURE_EXPR);
        self.bump(); // `|`
                     // Zero or more `name` or `name: Type` params separated by commas.
        if !self.at(SyntaxKind::PIPE) {
            loop {
                let before = self.meaningful_index();
                self.start_node(SyntaxKind::PARAM);
                self.expect_binder("closure parameter name");
                if self.eat(SyntaxKind::COLON) {
                    self.parse_type();
                }
                self.finish_node();
                if !self.eat(SyntaxKind::COMMA) {
                    break;
                }
                self.ensure_progress(before);
            }
        }
        self.expect(SyntaxKind::PIPE, "`|` to close closure parameters");
        // Body: a single expression (which may be a `{ block }`).
        self.parse_expr();
        self.finish_node();
    }

    /// Smallest expression: literals, names, calls, parenthesized expressions,
    /// blocks, `if`, `while`.
    fn parse_atom(&mut self) {
        let kind = self.peek();
        match kind {
            SyntaxKind::IntLit
            | SyntaxKind::FloatLit
            | SyntaxKind::TextLit
            | SyntaxKind::BacktickTemplate => {
                // Eat leading trivia *before* opening the node, so it attaches
                // to the enclosing context rather than nesting inside the literal.
                self.eat_trivia();
                self.start_node(SyntaxKind::LITERAL);
                // After eat_trivia the cursor is on the literal token; emit it
                // directly without another trivia sweep.
                self.bump_meaningful();
                self.finish_node();
            }
            SyntaxKind::KW_TRUE | SyntaxKind::KW_FALSE => {
                self.start_node(SyntaxKind::LITERAL);
                self.bump();
                self.finish_node();
            }
            SyntaxKind::L_PAREN => self.parse_paren(),
            SyntaxKind::L_BRACE => self.parse_block(),
            SyntaxKind::KW_IF => self.parse_if(),
            SyntaxKind::KW_WHILE => self.parse_while(),
            SyntaxKind::KW_FOR => self.parse_for(),
            SyntaxKind::KW_LOOP => self.parse_loop(),
            SyntaxKind::KW_BREAK => self.parse_break(),
            SyntaxKind::KW_CONTINUE => self.parse_continue(),
            SyntaxKind::KW_RETURN => self.parse_return(),
            SyntaxKind::KW_MATCH => self.parse_match(),
            SyntaxKind::Ident => self.parse_name_or_call(),
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

    fn parse_paren(&mut self) {
        // Either `( expr )` (PAREN_EXPR) or `( e1, e2, … )` (TUPLE_EXPR). We do
        // not know which until we see a comma after the first element, so take a
        // checkpoint *before* the `(`, emit the shared prefix, then retroactively
        // open the correct node kind at that checkpoint. Both `(` and `)` end up
        // inside the single resulting node (no double nesting).
        let cp = self.checkpoint_lhs();
        self.bump(); // `(`
        if self.at(SyntaxKind::R_PAREN) {
            // Empty `()`: a degenerate paren expr; type checking rejects it.
            self.expect(SyntaxKind::R_PAREN, "`)`");
            self.start_node_at(cp, SyntaxKind::PAREN_EXPR);
            self.finish_node();
            return;
        }
        self.parse_expr(); // first element
        let is_tuple = self.at(SyntaxKind::COMMA);
        let kind = if is_tuple {
            SyntaxKind::TUPLE_EXPR
        } else {
            SyntaxKind::PAREN_EXPR
        };
        self.start_node_at(cp, kind);
        if is_tuple {
            // Collect the remaining elements.
            loop {
                let before = self.meaningful_index();
                if !self.eat(SyntaxKind::COMMA) {
                    break;
                }
                if self.at(SyntaxKind::R_PAREN) {
                    // Trailing comma: `(a, b, )` — stop without another element.
                    break;
                }
                self.parse_expr();
                // Guarantee termination on any input.
                self.ensure_progress(before);
            }
        }
        self.expect(SyntaxKind::R_PAREN, "`)`");
        self.finish_node();
    }

    fn parse_if(&mut self) {
        self.start_node(SyntaxKind::IF_EXPR);
        self.bump(); // `if`
                     // The condition is a parenthesized or bare expression; accept either.
                     // Suppress record-literal parsing so `if x { … }` doesn't read
                     // the then-block as `x { … }`.
        let prev = self.no_struct_literal;
        self.no_struct_literal = true;
        self.parse_expr();
        self.no_struct_literal = prev;
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
        let prev = self.no_struct_literal;
        self.no_struct_literal = true;
        self.parse_expr();
        self.no_struct_literal = prev;
        self.parse_block();
        self.finish_node();
    }

    /// `for name in iter { body }` (M8, §4.11). The binding name and `in`
    /// keyword separate the iterator expression from the loop body.
    fn parse_for(&mut self) {
        self.start_node(SyntaxKind::FOR_EXPR);
        self.bump(); // `for`
        self.expect(SyntaxKind::Ident, "binding name after `for`");
        self.expect(SyntaxKind::KW_IN, "`in` after the for-loop binding");
        let prev = self.no_struct_literal;
        self.no_struct_literal = true;
        self.parse_expr(); // iterator
        self.no_struct_literal = prev;
        self.parse_block();
        self.finish_node();
    }

    /// `loop { body }` (M8, §4.11) — an explicit infinite loop, terminated by
    /// `break` (optionally with a value).
    fn parse_loop(&mut self) {
        self.start_node(SyntaxKind::LOOP_EXPR);
        self.bump(); // `loop`
        self.parse_block();
        self.finish_node();
    }

    /// `break [expr]` (M8, §4.11). The optional value is an expression; absent
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

    /// `continue` (M8, §4.11).
    fn parse_continue(&mut self) {
        self.start_node(SyntaxKind::CONTINUE_EXPR);
        self.bump(); // `continue`
        self.finish_node();
    }

    /// `return [expr]` (M8, §4.11).
    fn parse_return(&mut self) {
        self.start_node(SyntaxKind::RETURN_EXPR);
        self.bump(); // `return`
        if self.starts_expr() {
            self.parse_expr();
        }
        self.finish_node();
    }

    /// `match scrutinee { pattern => expr, … }` (M7, §4.6/§4.11).
    fn parse_match(&mut self) {
        self.start_node(SyntaxKind::MATCH_EXPR);
        self.bump(); // `match`
                     // The scrutinee is an expression; suppress record literals so the `{`
                     // opening the arm list isn't consumed as a record body.
        let prev = self.no_struct_literal;
        self.no_struct_literal = true;
        self.parse_expr();
        self.no_struct_literal = prev;
        self.expect(SyntaxKind::L_BRACE, "`{` to begin match arms");
        if !self.at(SyntaxKind::R_BRACE) {
            loop {
                let before = self.meaningful_index();
                self.start_node(SyntaxKind::MATCH_ARM);
                self.parse_pattern();
                self.expect(SyntaxKind::FAT_ARROW, "`=>` in match arm");
                // Arm body: suppress record literals so a Name { ... } in the
                // body isn't confused with the next arm's pattern.
                let prev = self.no_struct_literal;
                self.no_struct_literal = true;
                self.parse_expr();
                self.no_struct_literal = prev;
                self.finish_node(); // MATCH_ARM
                                    // Arms really are comma-OR-newline separated (§4.6) — the
                                    // newline half used to be a comment over a check that only
                                    // asked whether a pattern could start here (FE-04).
                let comma = self.eat(SyntaxKind::COMMA);
                // Stop if we hit `}` or something that can't start a pattern.
                if self.at(SyntaxKind::R_BRACE) || !is_pattern_start(self.peek()) {
                    break;
                }
                if !comma && !self.newline_before() {
                    let span = self.current_span();
                    self.error_with(
                        MISSING_SEPARATOR,
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

    /// Parse a pattern (M7, §4.6). M7 supports: `_` (wildcard), a literal
    /// (Int/Text/Bool), a variable bind (`x`), an enum variant (`Empty` or
    /// `Number(sub_pattern, …)`), and tuple/record patterns.
    fn parse_pattern(&mut self) {
        self.start_node(SyntaxKind::PATTERN);
        match self.peek() {
            SyntaxKind::UNDERSCORE => {
                self.bump(); // `_`
            }
            SyntaxKind::IntLit
            | SyntaxKind::TextLit
            | SyntaxKind::KW_TRUE
            | SyntaxKind::KW_FALSE => {
                self.bump(); // literal
            }
            SyntaxKind::Ident => {
                self.bump(); // variant name or variable bind
                             // Enum variant with payload: `Name(pat, pat, …)`.
                if self.eat(SyntaxKind::L_PAREN) {
                    if !self.at(SyntaxKind::R_PAREN) {
                        loop {
                            let before = self.meaningful_index();
                            self.parse_pattern();
                            if !self.eat(SyntaxKind::COMMA) {
                                break;
                            }
                            self.ensure_progress(before);
                        }
                    }
                    self.expect(SyntaxKind::R_PAREN, "`)` to close variant pattern");
                }
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

    // --- types (M2) ---------------------------------------------------------

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
    /// a collection type (M5, §4.4) — also emitted as `TYPE_REF` with the bracketed
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
                    self.parse_type();
                }
                self.expect(SyntaxKind::R_BRACK, "`]`");
                self.finish_node(); // wraps name + args into one TYPE_REF
            }
            return;
        }
        if self.at(SyntaxKind::L_PAREN) {
            // `( T )` (grouped) or `( T, U, … )` (tuple). Same checkpoint trick
            // as `parse_paren`: emit the shared prefix, then open the right kind.
            let cp = self.checkpoint_lhs();
            self.bump(); // `(`
            if self.at(SyntaxKind::R_PAREN) {
                // Empty `()` — degenerate; record as TYPE_REF and let type
                // resolution reject it.
                self.expect(SyntaxKind::R_PAREN, "`)`");
                self.start_node_at(cp, SyntaxKind::TYPE_REF);
                self.finish_node();
                return;
            }
            self.parse_type(); // first element
            let is_tuple = self.at(SyntaxKind::COMMA);
            let kind = if is_tuple {
                SyntaxKind::TUPLE_TYPE
            } else {
                SyntaxKind::TYPE_REF
            };
            self.start_node_at(cp, kind);
            if is_tuple {
                loop {
                    let before = self.meaningful_index();
                    if !self.eat(SyntaxKind::COMMA) {
                        break;
                    }
                    if self.at(SyntaxKind::R_PAREN) {
                        break; // trailing comma
                    }
                    self.parse_type();
                    self.ensure_progress(before);
                }
            }
            self.expect(SyntaxKind::R_PAREN, "`)`");
            self.finish_node();
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
    fn parse_name_or_call(&mut self) {
        let cp = self.checkpoint_lhs();
        // Peek the identifier text to special-case `parse(text, parser_expr)`
        // (§7.1) before committing to an ordinary call.
        let name_text = self.peek_text();
        let is_parse_call = name_text == Some("parse");
        self.start_node(SyntaxKind::PATH_EXPR);
        self.bump(); // name
        self.finish_node();
        if self.at(SyntaxKind::L_PAREN) {
            if is_parse_call {
                // `parse(text, parser_expression)` (§7.1). The first arg is an
                // ordinary expression (the Text to parse); the second is a
                // parser-expression.
                self.start_node_at(cp, SyntaxKind::PARSE_EXPR);
                self.bump(); // `(`
                self.parse_expr(); // first arg: the Text
                if self.eat(SyntaxKind::COMMA) {
                    self.parse_parser_expr();
                }
                self.expect(SyntaxKind::R_PAREN, "`)` to close parse()");
                self.finish_node(); // PARSE_EXPR
                return;
            }
            self.bump(); // `(`
            self.start_node_at(cp, SyntaxKind::CALL_EXPR);
            // Re-open the path as the callee: rowan's checkpoint wraps the
            // already-emitted PATH_EXPR, so the call's first child is the path.
            self.parse_arg_list();
            self.finish_node(); // CALL_EXPR
        } else if self.at(SyntaxKind::L_BRACE) && !self.no_struct_literal {
            // Record literal: `Name { field: expr, … }` or `Name { x, y }` (§4.5
            // punning). In expression position, a bare name followed by `{` is a
            // record construction (not a block — blocks only follow `if`/`while`
            // keywords or appear as `({ … })`).
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
    // Input-parser expression grammar (§7, M6).
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
            SyntaxKind::BacktickTemplate => self.parse_parser_template(),
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
    /// - a string literal (the separator for `sep`);
    /// - a named argument `name: parser_expr` (M9, §7.5), emitted as a
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
                if self.at(SyntaxKind::TextLit) {
                    // A string-literal separator for `sep`.
                    self.start_node(SyntaxKind::LITERAL);
                    self.bump();
                    self.finish_node();
                } else if self.at(SyntaxKind::Ident) && self.nth_kind(1) == SyntaxKind::COLON {
                    // A named argument `name: parser_expr` (M9). The name is a
                    // bare ident followed by `:`. This does not conflict with a
                    // constructor call (`lines(...)`) because that has `(` at
                    // position 1, not `:`.
                    self.start_node(SyntaxKind::PARSER_NAMED_ARG);
                    self.bump(); // the name ident
                    self.eat_trivia();
                    self.expect(SyntaxKind::COLON, "`:` after a named argument");
                    self.eat_trivia();
                    self.parse_parser_expr();
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

    // -----------------------------------------------------------------------
    // Error recovery.
    // -----------------------------------------------------------------------

    /// Consume `kind`; if it is absent, emit a diagnostic at the current token.
    /// Expect a **binding position**: a name, or `_` for one the program is
    /// deliberately not naming (D7, ADR-049).
    ///
    /// `let _ = f()`, `fn g(_)` and `|_| 0` are legal and introduce nothing —
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
                SyntaxKind::KW_LET
                    | SyntaxKind::KW_VAR
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
    fn checkpoint_lhs(&self) -> rowan::Checkpoint {
        self.builder.checkpoint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use praxis_source::SourceMap;

    fn parse_text(text: &str) -> ParseOutput {
        let map = SourceMap::new();
        let id = map.intern("test.px", text);
        parse(id, text)
    }

    fn dump(text: &str) -> String {
        praxis_test_support::format_syntax_tree(&parse_text(text).tree)
    }

    #[test]
    fn parses_let_binding() {
        let out = parse_text("let x = 1");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        insta::assert_snapshot!(dump("let x = 1"), @r#"
        SOURCE_FILE@0..9
          LET_STMT@0..9
            KW_LET "let"@0..3
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
        let out = parse_text("{ let a = 1\n a }");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::BLOCK_EXPR));
        assert!(kinds.contains(&SyntaxKind::LET_STMT));
        assert!(kinds.contains(&SyntaxKind::EXPR_STMT));
    }

    #[test]
    fn parses_text_literal() {
        let out = parse_text(r#"out("hello")"#);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::TextLit));
    }

    // --- 2026-07 adversarial audit regressions -----------------------------

    #[test]
    fn regression_same_line_statements_require_a_semicolon() {
        let out = parse_text("let a = 1 let b = 2");
        assert!(
            !out.diagnostics.is_empty(),
            "two statements on one line must not parse as if a separator existed"
        );
    }

    #[test]
    fn regression_semicolons_separate_top_level_statements() {
        let out = parse_text("let a = 1; let b = 2");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert_eq!(
            construct_names(&out.tree)
                .iter()
                .filter(|kind| **kind == SyntaxKind::LET_STMT)
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

    // --- FE-04: where a newline ends a statement, and where it does not -----

    /// D8's second half, stated for the operator it is most likely to break.
    /// A newline is consulted between statements and at `break`/`return`'s
    /// optional-value decision — never inside the Pratt loop.
    #[test]
    fn an_operator_continues_across_a_line_break() {
        let out = parse_text("let a = 1 +\n2");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == SyntaxKind::LET_STMT)
                .count(),
            1,
            "`1 +` and `2` are one addition, not two statements"
        );
        assert!(kinds.contains(&SyntaxKind::BIN_EXPR));
    }

    /// …and so does a postfix chain: the line break before `.len()` is inside an
    /// expression, so it terminates nothing.
    #[test]
    fn a_method_chain_continues_across_a_line_break() {
        let out = parse_text("let n = v\n  .len()");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert!(construct_names(&out.tree).contains(&SyntaxKind::METHOD_CALL_EXPR));
    }

    /// `break`'s half of the optional-value rule; the exit test covers `return`.
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

    /// A `;` still separates statements *inside* a block, where it was already
    /// consumed — the change is that it is no longer the only thing that can be.
    #[test]
    fn a_semicolon_separates_two_statements_on_one_line_in_a_block() {
        let out = parse_text("fn f() { let a = 1; let b = 2 }");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert_eq!(
            construct_names(&out.tree)
                .iter()
                .filter(|kind| **kind == SyntaxKind::LET_STMT)
                .count(),
            2
        );
    }

    /// A run-on is reported where it happens and parsing continues: three
    /// statements, two missing separators, three `LET_STMT`s and no cascade.
    #[test]
    fn each_missing_separator_is_reported_once_and_parsing_continues() {
        let out = parse_text("let a = 1 let b = 2 let c = 3");
        let separator_errors = out
            .diagnostics
            .iter()
            .filter(|d| d.code() == MISSING_SEPARATOR)
            .count();
        assert_eq!(separator_errors, 2, "{:?}", out.diagnostics);
        assert_eq!(
            construct_names(&out.tree)
                .iter()
                .filter(|kind| **kind == SyntaxKind::LET_STMT)
                .count(),
            3
        );
    }

    /// The block loop demands one too, and the closing `}` is a separator in its
    /// own right — a trailing expression needs nothing after it.
    #[test]
    fn a_block_demands_a_separator_but_its_closing_brace_is_one() {
        let clean = parse_text("fn f() { let a = 1\n a }");
        assert!(clean.diagnostics.is_empty(), "{:?}", clean.diagnostics);

        let run_on = parse_text("fn f() { let a = 1 a }");
        assert!(
            run_on
                .diagnostics
                .iter()
                .any(|d| d.code() == MISSING_SEPARATOR),
            "{:?}",
            run_on.diagnostics
        );
    }

    /// Match arms are comma-OR-newline separated, which until FE-04 was a
    /// comment above a check that only asked whether a pattern could start.
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
                .any(|d| d.code() == MISSING_SEPARATOR),
            "{:?}",
            run_on.diagnostics
        );
    }

    #[test]
    #[ignore = "known bug: the record-literal suppression flag leaks through parentheses"]
    fn regression_parenthesized_record_literal_is_valid_in_a_condition() {
        let out = parse_text("if (Point { x: 1 } == p) { 0 }");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert!(
            construct_names(&out.tree).contains(&SyntaxKind::RECORD_LIT_EXPR),
            "the parenthesized condition must retain its record literal"
        );
    }

    #[test]
    #[ignore = "known bug: record literals are unconditionally suppressed in match arm bodies"]
    fn regression_match_arm_may_return_a_record_literal() {
        let out = parse_text("match x { A => Point { x: 1 } }");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert!(
            construct_names(&out.tree).contains(&SyntaxKind::RECORD_LIT_EXPR),
            "the arm body must contain a record literal"
        );
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
        let src = "let x 1\n )\nlet = \n";
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

    // --- M2: real type annotations + tuples ---------------------------------

    #[test]
    fn parses_let_with_tuple_type_annotation() {
        let src = "let p: (Int, Int) = (1, 2)";
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
        let src = "let x: Int = 1";
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
        let src = "let f: Int -> Text -> Bool = panic";
        let out = parse_text(src);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        // Two FN_TYPE nodes for the nested arrow.
        let fn_type_count = kinds.iter().filter(|k| **k == SyntaxKind::FN_TYPE).count();
        assert_eq!(fn_type_count, 2);
    }

    // --- M6: input-parser expression syntax (§7) ---

    #[test]
    fn parses_read_atomic() {
        let out = parse_text("let v = read int");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::READ_EXPR));
        assert!(kinds.contains(&SyntaxKind::PARSER_EXPR));
        assert!(kinds.contains(&SyntaxKind::PARSER_ATOM));
    }

    #[test]
    fn parses_read_lines_of_int() {
        let out = parse_text("let v = read lines(int)");
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
        let out = parse_text("let v = read sections( lines( csv( int ) ) )");
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
        let out = parse_text("let v = read lines(`{x:int},{y:int}`)");
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
        let out = parse_text(r#"let v = read sep(" -> ", word)"#);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::READ_EXPR));
        assert!(kinds.contains(&SyntaxKind::PARSER_CALL));
        // The string-literal separator is inside the arg list.
        assert!(kinds.contains(&SyntaxKind::TextLit));
    }

    // --- M9: named arguments in parser constructor calls (§7.5) ---------------

    #[test]
    fn parses_named_args_in_sections() {
        // heterogeneous `sections(rules: ..., updates: ...)` — two named args.
        let out = parse_text("let v = read sections(rules: lines(int), updates: lines(int))");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::PARSER_NAMED_ARG));
        // Two named args + one positional arg-less... actually two named args.
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
            parse_text("let v = read sections(draws: csv(int), boards: repeated(matrix(int)))");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::PARSER_NAMED_ARG));
    }

    #[test]
    fn parses_keyword_arg_in_chars() {
        // `chars(one_of(...), skip: whitespace)` — a positional arg followed by
        // a named keyword arg.
        let out = parse_text("let v = read chars(one_of(\"LR\"), skip: whitespace)");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(kinds.contains(&SyntaxKind::PARSER_NAMED_ARG));
    }

    #[test]
    fn named_arg_does_not_shadow_constructor_call() {
        // A constructor call argument (`lines(int)`) has `(` at position 1, so
        // it is NOT mistaken for a named arg. Only `ident:` is.
        let out = parse_text("let v = read sections(lines(int))");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds = construct_names(&out.tree);
        assert!(
            !kinds.contains(&SyntaxKind::PARSER_NAMED_ARG),
            "positional constructor-call arg must not parse as a named arg"
        );
    }

    #[test]
    fn parses_parse_call() {
        let out = parse_text("let v = parse(sample, lines(int))");
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
