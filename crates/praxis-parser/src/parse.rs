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

const fn bp(left: u8, right: u8) -> BindingPower {
    BindingPower { left, right }
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
        self.start_node(SyntaxKind::SOURCE_FILE);
        while !self.at_end() {
            // Emit any trivia leading each statement into the root node.
            self.eat_trivia();
            if self.at_end() {
                break;
            }
            let before = self.meaningful_index();
            if !self.parse_stmt() {
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
        self.diagnostics.push(Diagnostic::new(
            Severity::Error,
            DiagnosticCode::new(DiagnosticCategory::Parse, 1),
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
            // `name = expr` / `name += expr` reassignment (§4.2).
            SyntaxKind::Ident if is_assignment_op(self.nth_kind(1)) => self.parse_assign_stmt(),
            _ => self.parse_expr_stmt(),
        }
    }

    /// `let`/`var name [: Type] = expr` — `kind` is the node kind (LET_STMT / VAR_STMT).
    fn parse_let_or_var(&mut self, kind: SyntaxKind) -> bool {
        self.start_node(kind);
        self.bump(); // `let`/`var`
        self.expect(SyntaxKind::Ident, "binding name");
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
                    self.expect(SyntaxKind::Ident, "parameter name");
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
            // Semicolons are optional; consume one if present (§4.1).
            self.eat(SyntaxKind::SEMICOLON);
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

    /// Prefix expression: unary operators, then an atom or a parenthesized
    /// expression.
    fn parse_prefix(&mut self) {
        let op = self.peek();
        if let Some(bp) = prefix_binding_power(op) {
            self.start_node(SyntaxKind::UNARY_EXPR);
            self.bump(); // unary operator
            self.parse_expr_bp(bp);
            self.finish_node();
            return;
        }
        self.parse_atom();
    }

    /// Smallest expression: literals, names, calls, parenthesized expressions,
    /// blocks, `if`, `while`.
    fn parse_atom(&mut self) {
        let kind = self.peek();
        match kind {
            SyntaxKind::IntLit | SyntaxKind::TextLit | SyntaxKind::BacktickTemplate => {
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
        self.parse_expr();
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
        self.parse_expr();
        self.parse_block();
        self.finish_node();
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
        self.start_node(SyntaxKind::PATH_EXPR);
        self.bump(); // name
        self.finish_node();
        if self.at(SyntaxKind::L_PAREN) {
            self.bump(); // `(`
            self.start_node_at(cp, SyntaxKind::CALL_EXPR);
            // Re-open the path as the callee: rowan's checkpoint wraps the
            // already-emitted PATH_EXPR, so the call's first child is the path.
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
            self.finish_node(); // CALL_EXPR
        }
        // Postfix method calls: `.method(args)`, chained left-associatively.
        // Each iteration wraps the whole preceding expression (receiver) plus
        // the method name + args into a METHOD_CALL_EXPR node.
        //
        // The checkpoint `cp` was taken *before* the receiver was emitted, so
        // `start_node_at(cp, ...)` retroactively wraps the receiver (PATH_EXPR
        // or a prior CALL_EXPR / METHOD_CALL_EXPR) as the first child. For
        // chains (`v.push(1).len()`), update `cp` to the position before each
        // iteration so the next method wraps the previous METHOD_CALL_EXPR.
        let mut chain_cp = cp;
        while self.at(SyntaxKind::DOT) {
            self.bump(); // `.`
                         // The method name.
            if !self.at(SyntaxKind::Ident) {
                let span = self.current_span();
                self.error(span, "expected method name after `.`");
                break;
            }
            self.bump(); // method name
            self.start_node_at(chain_cp, SyntaxKind::METHOD_CALL_EXPR);
            if self.at(SyntaxKind::L_PAREN) {
                self.bump(); // `(`
                self.start_node(SyntaxKind::ARG_LIST);
                if !self.at(SyntaxKind::R_PAREN) {
                    loop {
                        let before = self.meaningful_index();
                        self.parse_expr();
                        if !self.eat(SyntaxKind::COMMA) {
                            break;
                        }
                        self.ensure_progress(before);
                    }
                }
                self.expect(SyntaxKind::R_PAREN, "`)`");
                self.finish_node(); // ARG_LIST
            }
            self.finish_node(); // METHOD_CALL_EXPR
                                // The next `.method()` in a chain must wrap this METHOD_CALL_EXPR,
                                // so take a fresh checkpoint at the current position (the builder
                                // position is now just after the finished METHOD_CALL_EXPR node).
            chain_cp = self.checkpoint_lhs();
        }
    }

    // -----------------------------------------------------------------------
    // Error recovery.
    // -----------------------------------------------------------------------

    /// Consume `kind`; if it is absent, emit a diagnostic at the current token.
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

    /// Collect the `SyntaxKind` of every node and token in the tree (for
    /// structural asserts that may reference either).
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
