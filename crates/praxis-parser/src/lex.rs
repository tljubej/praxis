//! The Praxis lexer.
//!
//! Turns source text into a stream of [`Token`]s (each carrying a
//! [`SyntaxKind`] and a [`Span`]) plus diagnostics. It is lossless — trivia
//! (whitespace and comments) is kept as real tokens so the parser can fold them
//! into the rowan tree verbatim (§13.1, ADR-003).
//!
//! Design notes:
//! - **Longest match** for operators: `->`, `=>`, `==`, `!=`, `..=`, `+=`, …
//!   are recognized before their single-character prefixes.
//! - **Keywords** are split out of the identifier run via
//!   [`SyntaxKind::from_keyword`]; `out`, `panic`, type names, etc. stay plain
//!   identifiers (they are builtins, not keywords).
//! - A bad byte does not abort lexing: it emits a `T003` diagnostic and the
//!   lexer advances one byte so the rest of the file is still reported (§17.1,
//!   "multiple diagnostics from one malformed file").
//!
//! Diagnostic codes (`T0xx`, [`DiagnosticCategory::Lex`]):
//! - `T001` — unterminated block comment.
//! - `T002` — unterminated backtick template.
//! - `T003` — unexpected byte in source.
//! - `T004` — unterminated text literal.
//! - `T005` — invalid escape in text literal.

use praxis_source::{Diagnostic, DiagnosticCategory, DiagnosticCode, FileId, Severity, Span};
use praxis_syntax::{SyntaxKind, Token};

/// The result of lexing one source file: the token stream and any diagnostics.
///
/// Diagnostics are returned alongside tokens rather than via `Result` because a
/// single bad byte should not abort lexing the rest of the file — the LSP needs
/// to keep reporting problems past the first one (§17.1, "multiple diagnostics
/// from one malformed file").
#[derive(Debug)]
pub struct LexOutput {
    pub tokens: Vec<Token>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Lex `text` belonging to `file`, returning tokens and diagnostics.
///
/// This is the stable front-end entry point the CLI and parser both call; its
/// shape is deliberately unchanged from Milestone 0.
pub fn lex(file: FileId, text: &str) -> LexOutput {
    let mut lexer = Lexer::new(file, text);
    lexer.run();
    LexOutput {
        tokens: lexer.tokens,
        diagnostics: lexer.diagnostics,
    }
}

struct Lexer<'a> {
    file: FileId,
    src: &'a [u8],
    /// Current byte offset into `src`.
    pos: usize,
    tokens: Vec<Token>,
    diagnostics: Vec<Diagnostic>,
}

impl<'a> Lexer<'a> {
    fn new(file: FileId, text: &'a str) -> Lexer<'a> {
        Lexer {
            file,
            src: text.as_bytes(),
            pos: 0,
            tokens: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    fn run(&mut self) {
        while self.pos < self.src.len() {
            let start = self.pos;
            match self.classify_byte(self.src[start]) {
                ByteClass::Whitespace => self.eat_whitespace(),
                ByteClass::LineComment => self.eat_line_comment(),
                ByteClass::BlockComment => self.eat_block_comment(start),
                ByteClass::IdentStart => self.eat_ident(start),
                ByteClass::Digit => self.eat_int(start),
                ByteClass::Quote => self.eat_text(start),
                ByteClass::Punct => self.eat_punct(start),
                ByteClass::Backtick => self.eat_template(start),
                ByteClass::Unknown => self.diagnose_unknown(start),
            }
        }
        self.tokens
            .push(Token::new(SyntaxKind::EOF, Span::at(self.pos as u32)));
    }

    fn classify_byte(&self, b: u8) -> ByteClass {
        match b {
            b' ' | b'\t' | b'\n' | b'\r' => ByteClass::Whitespace,
            // A `/` is a comment only when followed by `/` or `*`; otherwise it
            // is punctuation (the division operator / part of a comment opener).
            b'/' if self.starts_with(b"//") => ByteClass::LineComment,
            b'/' if self.starts_with(b"/*") => ByteClass::BlockComment,
            b'_' | b'a'..=b'z' | b'A'..=b'Z' => ByteClass::IdentStart,
            b'0'..=b'9' => ByteClass::Digit,
            b'"' => ByteClass::Quote,
            b'`' => ByteClass::Backtick,
            // Any leading punctuation byte of an operator we recognize. The
            // precise multi-char split happens in `eat_punct`; the class just
            // routes the first byte here.
            b'(' | b')' | b'{' | b'}' | b'[' | b']' | b'<' | b'>' | b'+' | b'-' | b'*' | b'/'
            | b'%' | b'=' | b'!' | b'?' | b':' | b';' | b',' | b'.' | b'|' | b'&' | b'#' => {
                ByteClass::Punct
            }
            _ => ByteClass::Unknown,
        }
    }

    fn eat_whitespace(&mut self) {
        let start = self.pos;
        while self.pos < self.src.len()
            && matches!(self.src[self.pos], b' ' | b'\t' | b'\n' | b'\r')
        {
            self.pos += 1;
        }
        self.push(SyntaxKind::Whitespace, start);
    }

    fn eat_line_comment(&mut self) {
        let start = self.pos;
        self.pos += 2; // skip leading `//`
        while self.pos < self.src.len() && !matches!(self.src[self.pos], b'\n' | b'\r') {
            self.pos += 1;
        }
        self.push(SyntaxKind::LineComment, start);
    }

    fn eat_block_comment(&mut self, start: usize) {
        // Nestable block comments (§4.1).
        self.pos += 2; // skip leading `/*`
        let mut depth: u32 = 1;
        while self.pos < self.src.len() && depth > 0 {
            if self.starts_with(b"/*") {
                depth += 1;
                self.pos += 2;
            } else if self.starts_with(b"*/") {
                depth -= 1;
                self.pos += 2;
            } else {
                self.pos += 1;
            }
        }
        if depth > 0 {
            // Unterminated: emit a diagnostic but still emit the token so the
            // rest of the file can be processed.
            self.diagnostic(
                Span::new(start as u32, self.pos as u32),
                DiagnosticCode::new(DiagnosticCategory::Lex, 1),
                "unterminated block comment",
            );
        }
        self.push(SyntaxKind::BlockComment, start);
    }

    fn eat_ident(&mut self, start: usize) {
        // First byte is already known to be ident-start; advance and consume
        // XID-continue-ish bytes. ASCII is exact; non-ASCII is accepted
        // permissively (a proper XID table is a follow-up — see TODO below).
        self.pos += 1;
        while self.pos < self.src.len() && is_ident_continue(self.src[self.pos]) {
            self.pos += 1;
        }
        // Look up the keyword table: `let`/`if`/… become their own kinds; the
        // rest stay identifiers. Builtins (`out`, `panic`, type names) are
        // intentionally not keywords.
        let text = &self.src[start..self.pos];
        // SAFETY: the span came from a valid UTF-8 source slice, so it is UTF-8.
        let text = std::str::from_utf8(text).expect("ident slice is UTF-8");
        let kind = SyntaxKind::from_keyword(text).unwrap_or(SyntaxKind::Ident);
        self.push(kind, start);
    }

    fn eat_int(&mut self, start: usize) {
        while self.pos < self.src.len() && self.src[self.pos].is_ascii_digit() {
            self.pos += 1;
        }
        self.push(SyntaxKind::IntLit, start);
    }

    fn eat_text(&mut self, start: usize) {
        // Double-quoted text literal with `\` escapes (§4). The body is scanned
        // here only to find the closing quote; its semantic value is decoded
        // later. We do validate escapes so a stray trailing backslash is caught.
        self.pos += 1; // opening `"`
        while self.pos < self.src.len() {
            match self.src[self.pos] {
                b'"' => {
                    self.pos += 1; // closing quote
                    self.push(SyntaxKind::TextLit, start);
                    return;
                }
                b'\\' => {
                    // Need at least one more byte for the escape.
                    if self.pos + 1 >= self.src.len() {
                        break;
                    }
                    let esc = self.src[self.pos + 1];
                    if !is_valid_escape(esc) {
                        let bad_at = self.pos;
                        self.pos += 2;
                        self.diagnostic(
                            Span::new(bad_at as u32, self.pos as u32),
                            DiagnosticCode::new(DiagnosticCategory::Lex, 5),
                            "invalid escape in text literal",
                        );
                    } else {
                        self.pos += 2;
                    }
                }
                b'\n' | b'\r' => {
                    // A raw newline inside a text literal is not allowed; report
                    // it and let the unterminated path close the token at EOF.
                    break;
                }
                _ => self.pos += 1,
            }
        }
        // Reached EOF (or a newline) without a closing quote.
        self.diagnostic(
            Span::new(start as u32, self.pos as u32),
            DiagnosticCode::new(DiagnosticCategory::Lex, 4),
            "unterminated text literal",
        );
        self.push(SyntaxKind::TextLit, start);
    }

    fn eat_punct(&mut self, start: usize) {
        // Longest-match: try the three- and two-char operators first, then fall
        // back to single-byte punctuation. `match_op` advances `pos` past the
        // matched bytes and returns the kind.
        let kind = self
            .match_op()
            .unwrap_or_else(|| single_punct(self.src[start]).unwrap_or(SyntaxKind::ERROR));
        self.push(kind, start);
    }

    /// Try to match the longest operator beginning at `pos`, advancing `pos`
    /// past it. Returns the matched kind for multi-char operators, or `None`
    /// for a bare single-byte punctuation byte (the caller then falls back to
    /// [`single_punct`]).
    fn match_op(&mut self) -> Option<SyntaxKind> {
        // Three-char operators first (only `..=` so far), then two-char. Order
        // matters: longest first so `..=` is not misread as `..` then `=`.
        let three = self.src.get(self.pos..self.pos + 3);
        if let Some([b'.', b'.', b'=']) = three {
            self.pos += 3;
            return Some(SyntaxKind::DOT2EQ);
        }
        let two = self.src.get(self.pos..self.pos + 2);
        let matched = match two {
            Some(b"->") => Some(SyntaxKind::THIN_ARROW),
            Some(b"=>") => Some(SyntaxKind::FAT_ARROW),
            Some(b"==") => Some(SyntaxKind::EQ2),
            Some(b"!=") => Some(SyntaxKind::NEQ),
            Some(b"<=") => Some(SyntaxKind::LTEQ),
            Some(b">=") => Some(SyntaxKind::GTEQ),
            Some(b"..") => Some(SyntaxKind::DOT2),
            Some(b"||") => Some(SyntaxKind::PIPE2),
            Some(b"+=") => Some(SyntaxKind::PLUS_EQ),
            Some(b"-=") => Some(SyntaxKind::MINUS_EQ),
            Some(b"*=") => Some(SyntaxKind::STAR_EQ),
            Some(b"/=") => Some(SyntaxKind::SLASH_EQ),
            Some(b"%=") => Some(SyntaxKind::PERCENT_EQ),
            _ => None,
        };
        if matched.is_some() {
            self.pos += 2;
        } else {
            // Single-byte operator/punct. Advance one byte and signal "no
            // multi-char match" so the caller resolves the kind itself.
            self.pos += 1;
        }
        matched
    }

    fn eat_template(&mut self, start: usize) {
        // Consume until the matching closing backtick. The M6 template lexer
        // will re-scan the contents; for M1 the whole template is one token.
        self.pos += 1; // opening backtick
        while self.pos < self.src.len() && self.src[self.pos] != b'`' {
            // Honour `\\` so an escaped backtick doesn't terminate the template.
            if self.src[self.pos] == b'\\' && self.pos + 1 < self.src.len() {
                self.pos += 2;
            } else {
                self.pos += 1;
            }
        }
        if self.pos < self.src.len() {
            self.pos += 1; // closing backtick
        } else {
            self.diagnostic(
                Span::new(start as u32, self.pos as u32),
                DiagnosticCode::new(DiagnosticCategory::Lex, 2),
                "unterminated backtick template",
            );
        }
        self.push(SyntaxKind::BacktickTemplate, start);
    }

    fn diagnose_unknown(&mut self, start: usize) {
        // Advance one byte so we make progress.
        self.pos += 1;
        let span = Span::new(start as u32, self.pos as u32);
        self.diagnostic(
            span,
            DiagnosticCode::new(DiagnosticCategory::Lex, 3),
            "unexpected byte in source",
        );
    }

    // --- helpers ---

    fn push(&mut self, kind: SyntaxKind, start: usize) {
        self.tokens
            .push(Token::new(kind, Span::new(start as u32, self.pos as u32)));
    }

    fn diagnostic(&mut self, span: Span, code: DiagnosticCode, message: &str) {
        self.diagnostics.push(Diagnostic::new(
            Severity::Error,
            code,
            message,
            praxis_source::FileSpan::new(self.file, span),
        ));
    }

    fn starts_with(&self, needle: &[u8]) -> bool {
        self.src[self.pos..].starts_with(needle)
    }
}

/// Whether `b` may continue an identifier (after the first byte).
// TODO(M1 follow-up): replace the permissive `b >= 0x80` arm with a real Unicode
// XID-Continue table; for now non-ASCII bytes are accepted so UTF-8 identifiers
// lex without spurious errors.
fn is_ident_continue(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_') || b >= 0x80
}

/// The `SyntaxKind` for a single-byte punctuation/operator byte, or `None` if
/// the byte is not punctuation at all.
fn single_punct(b: u8) -> Option<SyntaxKind> {
    Some(match b {
        b'(' => SyntaxKind::L_PAREN,
        b')' => SyntaxKind::R_PAREN,
        b'{' => SyntaxKind::L_BRACE,
        b'}' => SyntaxKind::R_BRACE,
        b'[' => SyntaxKind::L_BRACK,
        b']' => SyntaxKind::R_BRACK,
        b',' => SyntaxKind::COMMA,
        b'.' => SyntaxKind::DOT,
        b':' => SyntaxKind::COLON,
        b';' => SyntaxKind::SEMICOLON,
        b'#' => SyntaxKind::HASH,
        b'|' => SyntaxKind::PIPE,
        b'&' => SyntaxKind::AMP,
        b'+' => SyntaxKind::PLUS,
        b'-' => SyntaxKind::MINUS,
        b'*' => SyntaxKind::STAR,
        b'/' => SyntaxKind::SLASH,
        b'%' => SyntaxKind::PERCENT,
        b'=' => SyntaxKind::EQ,
        b'!' => SyntaxKind::BANG,
        b'<' => SyntaxKind::LT,
        b'>' => SyntaxKind::GT,
        b'?' => SyntaxKind::QUESTION,
        _ => return None,
    })
}

/// Whether `esc` is a recognized escape character inside a text literal.
fn is_valid_escape(esc: u8) -> bool {
    matches!(esc, b'"' | b'\\' | b'n' | b'r' | b't' | b'0' | b'`')
}

#[derive(Clone, Copy)]
enum ByteClass {
    Whitespace,
    LineComment,
    BlockComment,
    IdentStart,
    Digit,
    Quote,
    Punct,
    Backtick,
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use praxis_source::SourceMap;

    fn lex_text(text: &str) -> (Vec<SyntaxKind>, Vec<Diagnostic>) {
        let map = SourceMap::new();
        let id = map.intern("test.px", text);
        let out = lex(id, text);
        (
            out.tokens.into_iter().map(|t| t.kind).collect(),
            out.diagnostics,
        )
    }

    #[test]
    fn clean_trivial_input_has_no_diagnostics() {
        let (kinds, diags) = lex_text("let x = 42 // hi\n");
        assert!(diags.is_empty(), "got diagnostics: {diags:?}");
        assert!(kinds.contains(&SyntaxKind::KW_LET)); // keyword split out
        assert!(kinds.contains(&SyntaxKind::Ident));
        assert!(kinds.contains(&SyntaxKind::IntLit));
        assert!(kinds.contains(&SyntaxKind::Whitespace));
        assert!(kinds.contains(&SyntaxKind::LineComment));
        assert!(kinds.last().is_some_and(|k| *k == SyntaxKind::EOF));
    }

    #[test]
    fn unknown_byte_emits_one_diagnostic() {
        let (kinds, diags) = lex_text("let @ = 1");
        assert_eq!(diags.len(), 1);
        assert_eq!(
            diags[0].code(),
            DiagnosticCode::new(DiagnosticCategory::Lex, 3)
        );
        // The `@` does not appear as a token, but lexing continues.
        assert!(!kinds.contains(&SyntaxKind::ERROR));
    }

    #[test]
    fn nested_block_comment() {
        let (kinds, diags) = lex_text("/* outer /* inner */ still outer */ x");
        assert!(diags.is_empty());
        assert!(kinds.contains(&SyntaxKind::BlockComment));
    }

    #[test]
    fn unterminated_block_comment_faults() {
        let (_, diags) = lex_text("/* never ends");
        assert_eq!(diags.len(), 1);
        assert_eq!(
            diags[0].code(),
            DiagnosticCode::new(DiagnosticCategory::Lex, 1)
        );
    }

    #[test]
    fn backtick_template_terminated() {
        let (kinds, diags) = lex_text("let p = `{x:int}`");
        assert!(diags.is_empty());
        assert!(kinds.contains(&SyntaxKind::BacktickTemplate));
    }

    #[test]
    fn unterminated_template_faults() {
        let (_, diags) = lex_text("let p = `never closes");
        assert_eq!(diags.len(), 1);
        assert_eq!(
            diags[0].code(),
            DiagnosticCode::new(DiagnosticCategory::Lex, 2)
        );
    }

    #[test]
    fn diagnostic_renders_for_unknown_byte() {
        let map = SourceMap::new();
        let id = map.intern("day.px", "let @ = 1");
        let out = lex(id, "let @ = 1");
        let rendered = praxis_source::render_one(&map, &out.diagnostics[0]);
        insta::assert_snapshot!(rendered, @r"
error[T003]: unexpected byte in source

  day.px:1:4
  1 | let @ = 1
    |     ^ unexpected byte in source
");
    }

    // ---- New M1 coverage ----

    #[test]
    fn keywords_split_from_identifiers() {
        let (kinds, diags) = lex_text("let var fn if else while for match return");
        assert!(diags.is_empty());
        assert!(kinds.contains(&SyntaxKind::KW_LET));
        assert!(kinds.contains(&SyntaxKind::KW_VAR));
        assert!(kinds.contains(&SyntaxKind::KW_FN));
        assert!(kinds.contains(&SyntaxKind::KW_IF));
        assert!(kinds.contains(&SyntaxKind::KW_ELSE));
        assert!(kinds.contains(&SyntaxKind::KW_WHILE));
        assert!(kinds.contains(&SyntaxKind::KW_FOR));
        assert!(kinds.contains(&SyntaxKind::KW_MATCH));
        assert!(kinds.contains(&SyntaxKind::KW_RETURN));
    }

    #[test]
    fn builtins_are_not_keywords() {
        // `out` and `panic` are builtin calls, and type names are identifiers.
        // Filter out trivia so the assertion is about the real tokens only.
        let (kinds, _) = lex_text("out panic Int Vec");
        let meaningful: Vec<_> = kinds
            .into_iter()
            .filter(|k| !k.is_trivia())
            .filter(|k| *k != SyntaxKind::EOF)
            .collect();
        assert!(
            meaningful.iter().all(|k| *k == SyntaxKind::Ident),
            "expected all identifiers, got {meaningful:?}"
        );
    }

    #[test]
    fn multi_char_operators_prefer_longest_match() {
        let (kinds, _) = lex_text("-> => == != <= >= += -= *= /= %= .. ..=");
        assert!(kinds.contains(&SyntaxKind::THIN_ARROW));
        assert!(kinds.contains(&SyntaxKind::FAT_ARROW));
        assert!(kinds.contains(&SyntaxKind::EQ2));
        assert!(kinds.contains(&SyntaxKind::NEQ));
        assert!(kinds.contains(&SyntaxKind::LTEQ));
        assert!(kinds.contains(&SyntaxKind::GTEQ));
        assert!(kinds.contains(&SyntaxKind::PLUS_EQ));
        assert!(kinds.contains(&SyntaxKind::MINUS_EQ));
        assert!(kinds.contains(&SyntaxKind::STAR_EQ));
        assert!(kinds.contains(&SyntaxKind::SLASH_EQ));
        assert!(kinds.contains(&SyntaxKind::PERCENT_EQ));
        assert!(kinds.contains(&SyntaxKind::DOT2));
        assert!(kinds.contains(&SyntaxKind::DOT2EQ));
        // The compound forms must NOT degrade into their single-char parts:
        // there is no standalone `=`, `<`, `>`, `.`, `+`, `-`, `*`, `/`, `%`
        // anywhere in the input.
        assert!(!kinds.contains(&SyntaxKind::EQ));
        assert!(!kinds.contains(&SyntaxKind::LT));
        assert!(!kinds.contains(&SyntaxKind::GT));
        assert!(!kinds.contains(&SyntaxKind::DOT));
        assert!(!kinds.contains(&SyntaxKind::PLUS));
        assert!(!kinds.contains(&SyntaxKind::MINUS));
        assert!(!kinds.contains(&SyntaxKind::STAR));
        assert!(!kinds.contains(&SyntaxKind::SLASH));
        assert!(!kinds.contains(&SyntaxKind::PERCENT));
    }

    #[test]
    fn single_punct_classifies() {
        let (kinds, _) = lex_text("( ) { } [ ] , : ; | + - * / % = ! < > ?");
        for k in [
            SyntaxKind::L_PAREN,
            SyntaxKind::R_PAREN,
            SyntaxKind::L_BRACE,
            SyntaxKind::R_BRACE,
            SyntaxKind::L_BRACK,
            SyntaxKind::R_BRACK,
            SyntaxKind::COMMA,
            SyntaxKind::COLON,
            SyntaxKind::SEMICOLON,
            SyntaxKind::PIPE,
            SyntaxKind::PLUS,
            SyntaxKind::MINUS,
            SyntaxKind::STAR,
            SyntaxKind::SLASH,
            SyntaxKind::PERCENT,
            SyntaxKind::EQ,
            SyntaxKind::BANG,
            SyntaxKind::LT,
            SyntaxKind::GT,
            SyntaxKind::QUESTION,
        ] {
            assert!(kinds.contains(&k), "missing punct kind {k:?}");
        }
    }

    #[test]
    fn pipe2_is_one_token() {
        let (kinds, _) = lex_text("||");
        assert!(kinds.contains(&SyntaxKind::PIPE2));
    }

    #[test]
    fn text_literal_terminates() {
        let (kinds, diags) = lex_text("\"hello\\nworld\"");
        assert!(diags.is_empty());
        assert!(kinds.contains(&SyntaxKind::TextLit));
    }

    #[test]
    fn unterminated_text_literal_faults() {
        let (_, diags) = lex_text("\"never closes");
        assert_eq!(diags.len(), 1);
        assert_eq!(
            diags[0].code(),
            DiagnosticCode::new(DiagnosticCategory::Lex, 4)
        );
    }

    #[test]
    fn invalid_escape_faults() {
        let (_, diags) = lex_text("\"bad \\q escape\"");
        assert_eq!(diags.len(), 1);
        assert_eq!(
            diags[0].code(),
            DiagnosticCode::new(DiagnosticCategory::Lex, 5)
        );
    }

    #[test]
    fn division_operator_not_a_comment() {
        // A lone `/` not followed by `/` or `*` is division.
        let (kinds, _) = lex_text("a / b");
        assert!(kinds.contains(&SyntaxKind::SLASH));
        assert!(!kinds.contains(&SyntaxKind::LineComment));
        assert!(!kinds.contains(&SyntaxKind::BlockComment));
    }

    #[test]
    fn eof_span_is_at_end() {
        let map = SourceMap::new();
        let id = map.intern("test.px", "ab");
        let out = lex(id, "ab");
        let eof = out.tokens.last().expect("eof token present");
        assert_eq!(eof.kind, SyntaxKind::EOF);
        assert_eq!(eof.span, Span::new(2, 2));
    }
}
