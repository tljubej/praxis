//! The Milestone 0 lexer stub.
//!
//! Scope is intentionally narrow (the full lexer is M1):
//! - Recognize whitespace, `//` line comments, and nestable `/* */` block
//!   comments as trivia.
//! - Recognize identifiers (`[A-Za-z_][A-Za-z0-9_]*` plus Unicode XID continue),
//!   integer literals, common punctuation, and backtick parser templates.
//! - Emit a real [`Diagnostic`] for any byte that does not match the above.
//!
//! Every token carries a [`Span`]; the stub does not yet produce a lossless
//! tree, but the spans are correct so diagnostics point at the right place.

use praxis_source::{Diagnostic, DiagnosticCategory, DiagnosticCode, FileId, Severity, Span};
use praxis_syntax::{Token, TokenKind};

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
                ByteClass::Punct => self.eat_punct(start),
                ByteClass::Backtick => self.eat_template(start),
                ByteClass::Unknown => self.diagnose_unknown(start),
            }
        }
        self.tokens
            .push(Token::new(TokenKind::Eof, Span::at(self.pos as u32)));
    }

    fn classify_byte(&self, b: u8) -> ByteClass {
        match b {
            b' ' | b'\t' | b'\n' | b'\r' => ByteClass::Whitespace,
            // A `/` is a comment only when followed by `/` or `*`; otherwise it
            // is punctuation (the division operator).
            b'/' if self.starts_with(b"//") => ByteClass::LineComment,
            b'/' if self.starts_with(b"/*") => ByteClass::BlockComment,
            b'_' | b'a'..=b'z' | b'A'..=b'Z' => ByteClass::IdentStart,
            b'0'..=b'9' => ByteClass::Digit,
            // Punctuation we know the language uses (§4.1, §4.5, §4.6, §7).
            b'(' | b')' | b'{' | b'}' | b'[' | b']' | b'<' | b'>' | b'+' | b'-' | b'*' | b'/'
            | b'%' | b'=' | b'!' | b'?' | b':' | b';' | b',' | b'.' | b'|' | b'&' | b'^' | b'~' => {
                ByteClass::Punct
            }
            b'`' => ByteClass::Backtick,
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
        self.push(TokenKind::Whitespace, start);
    }

    fn eat_line_comment(&mut self) {
        let start = self.pos;
        self.pos += 2; // skip leading `//`
        while self.pos < self.src.len() && !matches!(self.src[self.pos], b'\n' | b'\r') {
            self.pos += 1;
        }
        self.push(TokenKind::LineComment, start);
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
        self.push(TokenKind::BlockComment, start);
    }

    fn eat_ident(&mut self, start: usize) {
        // First byte is already known to be ident-start; advance and consume
        // XID-continue-ish bytes. ASCII is exact; non-ASCII is accepted
        // permissively (a proper XID table lands in M1).
        self.pos += 1;
        while self.pos < self.src.len() && is_ident_continue(self.src[self.pos]) {
            self.pos += 1;
        }
        self.push(TokenKind::Ident, start);
    }

    fn eat_int(&mut self, start: usize) {
        while self.pos < self.src.len() && self.src[self.pos].is_ascii_digit() {
            self.pos += 1;
        }
        self.push(TokenKind::IntLit, start);
    }

    fn eat_punct(&mut self, start: usize) {
        // Collapse a run of consecutive punct bytes into one token; M1 will
        // split multi-char operators precisely.
        self.pos += 1;
        while self.pos < self.src.len() && is_punct(self.src[self.pos]) {
            self.pos += 1;
        }
        self.push(TokenKind::Punct, start);
    }

    fn eat_template(&mut self, start: usize) {
        // Consume until the matching closing backtick. The M6 template lexer
        // will re-scan the contents.
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
        self.push(TokenKind::BacktickTemplate, start);
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

    fn push(&mut self, kind: TokenKind, start: usize) {
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

fn is_ident_continue(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_') || b >= 0x80
}

fn is_punct(b: u8) -> bool {
    matches!(
        b,
        b'(' | b')'
            | b'{'
            | b'}'
            | b'['
            | b']'
            | b'<'
            | b'>'
            | b'+'
            | b'-'
            | b'*'
            | b'/'
            | b'%'
            | b'='
            | b'!'
            | b'?'
            | b':'
            | b';'
            | b','
            | b'.'
            | b'|'
            | b'&'
            | b'^'
            | b'~'
    )
}

#[derive(Clone, Copy)]
enum ByteClass {
    Whitespace,
    LineComment,
    BlockComment,
    IdentStart,
    Digit,
    Punct,
    Backtick,
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use praxis_source::SourceMap;

    fn lex_text(text: &str) -> (Vec<TokenKind>, Vec<Diagnostic>) {
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
        assert!(kinds.contains(&TokenKind::Ident));
        assert!(kinds.contains(&TokenKind::IntLit));
        assert!(kinds.contains(&TokenKind::Whitespace));
        assert!(kinds.contains(&TokenKind::LineComment));
        assert!(kinds.last().unwrap() == &TokenKind::Eof);
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
        assert!(!kinds.contains(&TokenKind::Unknown));
    }

    #[test]
    fn nested_block_comment() {
        let (kinds, diags) = lex_text("/* outer /* inner */ still outer */ x");
        assert!(diags.is_empty());
        assert!(kinds.contains(&TokenKind::BlockComment));
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
        assert!(kinds.contains(&TokenKind::BacktickTemplate));
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
        insta::assert_snapshot!(rendered, @r#"
        error[T003]: unexpected byte in source

          day.px:1:4
          1 | let @ = 1
            |     ^
        "#);
    }
}
