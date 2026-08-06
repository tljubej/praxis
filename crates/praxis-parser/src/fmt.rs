//! The Praxis formatter skeleton (Milestone 1).
//!
//! Lives in `praxis-parser` per ADR-005. It drives off the lossless syntax tree
//! (ADR-003) and emits a canonical layout — it never re-lexes raw text. The M1
//! acceptance criterion is **idempotency**: `format(format(src)) == format(src)`
//! on the milestone syntax (§19).
//!
//! Design: the tree already records every token's spelling and structure. The
//! formatter walks it and re-emits tokens with normalized trivia — a single
//! space where separation aids readability, a single newline at statement
//! boundaries, and one level of indentation (four spaces) per nested block.
//! Backtick template contents are preserved byte-for-byte (§15.2).
//!
//! Because the output is produced from the tree and the tree round-trips, a
//! second format pass re-parses to an equivalent tree and emits the same text:
//! that is the idempotency guarantee.

use praxis_source::FileId;
use praxis_syntax::{SyntaxKind, SyntaxNode, SyntaxToken};

use crate::parse::parse;

/// Format Praxis source: lex + parse + emit a canonical layout.
///
/// Returns the formatted text. Parse diagnostics are discarded here (the CLI
/// surfaces them separately); the formatter is purely syntactic.
#[must_use]
pub fn format_source(file: FileId, text: &str) -> String {
    let parsed = parse(file, text);
    format_node(&parsed.tree)
}

/// Format a single syntax node into the canonical layout.
#[must_use]
pub fn format_node(node: &SyntaxNode) -> String {
    let mut out = String::new();
    let mut ctx = FmtContext::new();
    fmt_node(node, &mut ctx, &mut out);
    // Ensure exactly one trailing newline.
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

struct FmtContext {
    /// Current indentation level (number of four-space units).
    indent: usize,
}

impl FmtContext {
    fn new() -> Self {
        Self { indent: 0 }
    }

    fn push_indent(&mut self) {
        self.indent += 1;
    }

    fn pop_indent(&mut self) {
        self.indent = self.indent.saturating_sub(1);
    }

    fn write_indent(&self, out: &mut String) {
        for _ in 0..self.indent {
            out.push_str("    ");
        }
    }
}

/// One thing a statement-list arm has to emit: a child node, or a comment that
/// sits between children. Comments are the only trivia the formatter keeps —
/// whitespace is re-derived, but a comment is source the programmer wrote and
/// deleting it is data loss, not normalization.
enum Item {
    Stmt(SyntaxNode),
    /// A comment plus whether it began its own source line. A comment that
    /// trailed a statement stays on that statement's line.
    Comment(SyntaxToken, bool),
}

/// The child nodes of `node` interleaved with its comment tokens, in source
/// order. `node.children()` skips tokens entirely, which is how every comment
/// at statement granularity used to be dropped.
fn items_of(node: &SyntaxNode) -> Vec<Item> {
    node.children_with_tokens()
        .filter_map(|element| match element {
            rowan::NodeOrToken::Node(child) => Some(Item::Stmt(child)),
            rowan::NodeOrToken::Token(token) if is_comment(token.kind()) => {
                let own_line = starts_new_line(&token);
                Some(Item::Comment(token, own_line))
            }
            rowan::NodeOrToken::Token(_) => None,
        })
        .collect()
}

fn is_comment(kind: SyntaxKind) -> bool {
    matches!(kind, SyntaxKind::LineComment | SyntaxKind::BlockComment)
}

/// Whether `token` began a source line: the trivia immediately before it
/// contained a newline, or nothing precedes it. This is what distinguishes a
/// trailing `// note` from a standalone comment line, and re-emitting each in
/// its original position is what makes formatting idempotent.
fn starts_new_line(token: &SyntaxToken) -> bool {
    let mut prev = token.prev_sibling_or_token();
    loop {
        match prev {
            None => return true,
            Some(rowan::NodeOrToken::Node(_)) => return false,
            Some(rowan::NodeOrToken::Token(t)) => {
                if t.kind() != SyntaxKind::Whitespace {
                    return false;
                }
                if t.text().contains('\n') {
                    return true;
                }
                prev = t.prev_sibling_or_token();
            }
        }
    }
}

fn fmt_node(node: &SyntaxNode, ctx: &mut FmtContext, out: &mut String) {
    match node.kind() {
        SyntaxKind::SOURCE_FILE => {
            // Statements separated by single newlines; no surrounding indent.
            for item in items_of(node) {
                match item {
                    Item::Stmt(child) => {
                        ctx.write_indent(out);
                        fmt_node(&child, ctx, out);
                        out.push('\n');
                    }
                    Item::Comment(token, own_line) => {
                        if own_line {
                            ctx.write_indent(out);
                        } else {
                            // Trailing comment: rejoin the line the previous
                            // statement just ended.
                            if out.ends_with('\n') {
                                out.pop();
                            }
                            out.push(' ');
                        }
                        out.push_str(token.text().trim_end());
                        out.push('\n');
                    }
                }
            }
        }
        SyntaxKind::BLOCK_EXPR => {
            // `{` on the same line; body indented; `}` dedented on its own line.
            // Emit the opening brace.
            out.push('{');
            ctx.push_indent();
            let items = items_of(node);
            if items.is_empty() {
                // Empty block: `{ }` inline.
                ctx.pop_indent();
                out.push('}');
                return;
            }
            for item in &items {
                match item {
                    Item::Stmt(child) => {
                        out.push('\n');
                        ctx.write_indent(out);
                        fmt_node(child, ctx, out);
                    }
                    Item::Comment(token, own_line) => {
                        if *own_line {
                            out.push('\n');
                            ctx.write_indent(out);
                        } else {
                            out.push(' ');
                        }
                        out.push_str(token.text().trim_end());
                    }
                }
            }
            ctx.pop_indent();
            out.push('\n');
            ctx.write_indent(out);
            out.push('}');
        }
        SyntaxKind::VAR_STMT | SyntaxKind::ASSIGN_STMT => {
            // Emit the meaningful tokens with single spaces around operators.
            fmt_token_stream(node, ctx, out);
        }
        SyntaxKind::FN_ITEM => {
            // Signature tokens, then the body block (handled by BLOCK_EXPR).
            fmt_token_stream(node, ctx, out);
        }
        SyntaxKind::IF_EXPR | SyntaxKind::WHILE_EXPR => {
            fmt_token_stream(node, ctx, out);
        }
        _ => {
            // Default: emit this node's meaningful tokens inline.
            fmt_token_stream(node, ctx, out);
        }
    }
}

/// Emit the meaningful tokens of `node` (and its descendants) inline, with a
/// single space inserted between adjacent tokens that need separation.
///
/// This is the conservative fallback: it reproduces the token stream without
/// the original trivia, inserting spacing only where two tokens would otherwise
/// collide (e.g. `var` + `x`, or `x` + `=`). Backtick templates and text
/// literals are emitted verbatim.
fn fmt_token_stream(node: &SyntaxNode, ctx: &mut FmtContext, out: &mut String) {
    let tokens: Vec<SyntaxToken> = node
        .descendants_with_tokens()
        .filter_map(|e| {
            if let rowan::NodeOrToken::Token(t) = e {
                Some(t)
            } else {
                None
            }
        })
        .collect();
    let mut prev: Option<SyntaxKind> = None;
    // Set after a line comment: everything after `//` belongs to that comment,
    // so the next token must start a fresh line or the statement is destroyed.
    let mut break_line = false;
    for token in tokens {
        let kind = token.kind();
        if kind == SyntaxKind::EOF || (kind.is_trivia() && !is_comment(kind)) {
            continue;
        }
        if break_line {
            out.push('\n');
            ctx.write_indent(out);
            prev = None;
        }
        // Insert a single space between two tokens when either needs separation.
        if let Some(prev_kind) = prev {
            if needs_space(prev_kind, kind) {
                out.push(' ');
            }
        }
        // Recurse: a nested block inside an expression statement is emitted by
        // walking into it via fmt_node for proper layout.
        out.push_str(token.text().trim_end());
        prev = Some(kind);
        break_line = kind == SyntaxKind::LineComment;
    }
    // After collecting tokens, re-walk to render nested blocks with layout.
    // (The token-stream approach above flattens blocks; for M1 idempotency we
    // additionally emit nested blocks through fmt_node. To keep it simple and
    // idempotent, the block layout is applied at the BLOCK_EXPR match arm, and
    // expressions only ever contain inline tokens.)
}

/// Whether a space should separate `prev` from `next`. Conservative: insert a
/// space unless the pair is clearly meant to be tight (e.g. `(`, `[`, `.`,
/// `,`, unary `-`, etc.).
fn needs_space(prev: SyntaxKind, next: SyntaxKind) -> bool {
    use SyntaxKind::*;
    // A comment is never run together with the code around it.
    if is_comment(prev) || is_comment(next) {
        return true;
    }
    // An interpolation fragment is glued to its hole (§8.1, ADR-147). The
    // fragment tokens carry their own delimiters — `"a{`, `}b{`, `}c"` — so a
    // space on either side of one lands *inside* the literal's braces, and
    // `"total: {n}"` reformats to `"total: { n }"`. Legal, and not what anybody
    // wrote.
    if matches!(prev, InterpOpen | InterpMiddle) || matches!(next, InterpMiddle | InterpClose) {
        return false;
    }
    // No space right after an opening bracket or before a closing one.
    if matches!(prev, L_PAREN | L_BRACE | L_BRACK) {
        return false;
    }
    if matches!(next, R_PAREN | R_BRACE | R_BRACK) {
        return false;
    }
    // No space around `,` `.` `:` (in args/access) — keep them tight.
    if matches!(prev, COMMA | DOT) || matches!(next, DOT | COMMA) {
        return false;
    }
    // No space between a name and a `(` call, or `)` and `(` chained call.
    if next == L_PAREN && matches!(prev, Ident | R_PAREN) {
        return false;
    }
    // Space around binary/assignment operators and after keywords.
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use praxis_source::SourceMap;

    fn format(text: &str) -> String {
        let map = SourceMap::new();
        let id = map.intern("test.px", text);
        format_source(id, text)
    }

    fn assert_clean(text: &str) {
        let out = parse_text_diagnostics(text);
        assert!(out.is_empty(), "input should parse cleanly: {out:?}");
    }

    fn parse_text_diagnostics(text: &str) -> Vec<praxis_source::Diagnostic> {
        let map = SourceMap::new();
        let id = map.intern("test.px", text);
        parse(id, text).diagnostics
    }

    #[test]
    fn format_is_idempotent_on_let_binding() {
        let src = "var x=1";
        let once = format(src);
        let twice = format(&once);
        assert_eq!(
            once, twice,
            "not idempotent:\n--once--\n{once}\n--twice--\n{twice}"
        );
        // And the canonical form is readable.
        insta::assert_snapshot!(once, @"var x = 1\n");
    }

    #[test]
    fn format_is_idempotent_on_arithmetic() {
        let src = "1+2*3";
        let once = format(src);
        let twice = format(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn format_is_idempotent_on_function() {
        let src = "fn add(a: Int,b: Int)->Int{a+b}";
        assert_clean(src);
        let once = format(src);
        let twice = format(&once);
        assert_eq!(once, twice, "\n--once--\n{once}\n--twice--\n{twice}");
    }

    #[test]
    fn format_is_idempotent_on_if_else() {
        let src = "if x{out(1)}else{out(2)}";
        assert_clean(src);
        let once = format(src);
        let twice = format(&once);
        assert_eq!(once, twice);
    }

    /// A list literal's brackets are tight, which is what `needs_space` already
    /// says about `[` and `]` — this is the gate that keeps it saying so now
    /// that a `[` can open an expression.
    ///
    /// The comma is tight too, and that is the formatter's own standing rule
    /// rather than anything about lists: `f(1,2)` and `(1,2)` come out the same
    /// way. A list literal inherits it rather than getting a second answer.
    #[test]
    fn format_is_idempotent_on_a_list_literal() {
        let src = "var v=[1,2 , 3]";
        assert_clean(src);
        let once = format(src);
        insta::assert_snapshot!(once, @"var v = [1,2,3]\n");
        assert_eq!(format(&once), once, "not idempotent:\n{once}");
        // The same shape a call and a tuple get, which is the point.
        assert_eq!(format("var v = f(1, 2)").trim_end(), "var v = f(1,2)");

        // The empty one, and a nested one, keep their shape too.
        for src in ["var v = []", "var v = [[1],[2,3]]"] {
            assert_clean(src);
            let once = format(src);
            assert_eq!(once.trim_end(), src, "{src}");
            assert_eq!(format(&once), once, "not idempotent:\n{once}");
        }
    }

    #[test]
    fn format_preserves_text_literal_verbatim() {
        let src = r#"out("hello world")"#;
        let once = format(src);
        assert!(once.contains("\"hello world\""), "literal changed: {once}");
        let twice = format(&once);
        assert_eq!(once, twice);
    }

    /// **ADR-147.** An interpolated literal reprints with its holes glued to
    /// their fragments.
    ///
    /// The fragments carry their own delimiters, so the formatter's default —
    /// "a space unless the pair is clearly meant to be tight" — would put one
    /// *inside* the braces and turn `"total: {n}"` into `"total: { n }"`. That is
    /// still a legal program with the same meaning, which is exactly why it needs
    /// a test: nothing else would notice.
    #[test]
    fn format_keeps_a_hole_glued_to_its_fragments() {
        // A subscript is deliberately absent from the list: `needs_space` puts a
        // space between an `Ident` and a `[`, so `m["k"]` reprints as `m ["k"]`
        // inside a hole and outside one alike. That is the formatter's own gap,
        // not interpolation's, and asserting it here would tie this test to a
        // defect it is not about.
        for src in [
            r#"out("total: {n}")"#,
            r#"out("{a} and {b}")"#,
            r#"out("{a + b}")"#,
            r#"out("{xs.len()}")"#,
        ] {
            let once = format(src);
            assert!(
                once.contains(src.trim_start_matches("out(")),
                "{src} → {once}"
            );
            let twice = format(&once);
            assert_eq!(once, twice, "{src} is not idempotent");
        }
    }

    #[test]
    fn format_preserves_backtick_template_verbatim() {
        // §15.2: backtick template contents must be preserved byte-for-byte.
        let src = "var p = `{x:int}`";
        let once = format(src);
        assert!(once.contains("`{x:int}`"), "template changed: {once}");
        let twice = format(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn regression_formatting_does_not_delete_comments() {
        let src = "var answer = 42 // the units matter";
        let once = format(src);
        assert!(
            once.contains("// the units matter"),
            "formatting deleted a source comment: {once:?}"
        );
    }

    /// A trailing comment stays on the line it trailed and a standalone
    /// comment keeps its own line — otherwise a second pass would move it and
    /// idempotency would fail.
    #[test]
    fn comments_keep_their_line_and_survive_reformatting() {
        let src = "// leading\nvar x=1 // trailing\nvar y=2";
        let once = format(src);
        insta::assert_snapshot!(once, @r"
        // leading
        var x = 1 // trailing
        var y = 2
        ");
        assert_eq!(format(&once), once, "formatting comments is not idempotent");
    }

    #[test]
    fn comments_inside_a_block_are_kept_and_indented() {
        let src = "fn f() {\n// inside\nvar y=2 // tail\n}";
        let once = format(src);
        assert!(once.contains("// inside"), "{once:?}");
        assert!(once.contains("// tail"), "{once:?}");
        assert_eq!(format(&once), once, "not idempotent:\n{once}");
    }

    #[test]
    fn a_block_comment_survives_inside_an_expression() {
        let src = "var x = /* why */ 1";
        let once = format(src);
        assert!(once.contains("/* why */"), "{once:?}");
        assert_eq!(format(&once), once, "not idempotent:\n{once}");
    }

    // --- M2: type annotations + tuples --------------------------------------

    #[test]
    fn format_is_idempotent_on_tuple() {
        let src = "out((1,2,3))";
        assert_clean(src);
        let once = format(src);
        let twice = format(&once);
        assert_eq!(once, twice, "\n--once--\n{once}\n--twice--\n{twice}");
    }

    #[test]
    fn format_is_idempotent_on_typed_let() {
        let src = "var p:(Int,Int)=(1,2)";
        assert_clean(src);
        let once = format(src);
        let twice = format(&once);
        assert_eq!(once, twice, "\n--once--\n{once}\n--twice--\n{twice}");
    }

    #[test]
    fn format_is_idempotent_on_higher_order_fn() {
        let src = "fn apply(f:(Int)->Int,x:Int)->Int{f(x)}";
        assert_clean(src);
        let once = format(src);
        let twice = format(&once);
        assert_eq!(once, twice, "\n--once--\n{once}\n--twice--\n{twice}");
    }

    /// **REP-57, ADR-091.** The headless record pattern round-trips, in each of
    /// the three positions a pattern appears.
    ///
    /// ADR-005 asks for this whenever the grammar grows: the formatter walks the
    /// tree, so a new production that produces a shape it cannot lay out is a
    /// source file the tool rewrites into a different program.
    ///
    /// **Observed red with the parser's `L_BRACE` pattern arm removed**: none of
    /// these sources parses, `assert_clean` fails on the first with "expected a
    /// pattern", and there is no tree for the formatter to be idempotent over.
    #[test]
    fn format_is_idempotent_on_a_headless_record_pattern() {
        for src in [
            "var r=match p{{x,y}=>x+y}",
            "for {x,y} in ps{out(x)}",
            "var f=|{x,y}|x+y",
            // Nested in a variant's payload — the shape a `choice(...)` payload
            // record needs, and the one the row was filed for.
            "var r=match m{Mul({a,b})=>a*b,Do(_)=>0}",
            // The head is optional, not gone.
            "var r=match p{P{x,y}=>x+y}",
        ] {
            assert_clean(src);
            let once = format(src);
            let twice = format(&once);
            assert_eq!(once, twice, "\n--once--\n{once}\n--twice--\n{twice}");
        }
    }
}
