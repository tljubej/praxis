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
//! - **Identifiers** are Unicode (§4.1): classification is per *scalar*, using
//!   the workspace-wide [`praxis_syntax::ident`] predicates, so a leading
//!   Unicode letter starts a name and a UTF-8 continuation byte cannot extend
//!   one.
//! - A bad scalar does not abort lexing: it emits a `T003` diagnostic and the
//!   lexer advances one whole scalar so the rest of the file is still reported
//!   (§17.1, "multiple diagnostics from one malformed file").
//!
//! Diagnostic codes (`T0xx`, [`DiagnosticCategory::Lex`]):
//! - `T001` — unterminated block comment.
//! - `T002` — unterminated backtick template.
//! - `T003` — unexpected character in source.
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
    /// The source as text. Byte indexing goes through [`Lexer::bytes`]; keeping
    /// the `&str` is what lets `eat_ident` decode scalars and slice the
    /// identifier run without a `from_utf8` round-trip.
    src: &'a str,
    /// Current byte offset into `src`.
    pos: usize,
    tokens: Vec<Token>,
    diagnostics: Vec<Diagnostic>,
}

impl<'a> Lexer<'a> {
    fn new(file: FileId, text: &'a str) -> Lexer<'a> {
        Lexer {
            file,
            src: text,
            pos: 0,
            tokens: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    fn run(&mut self) {
        while self.pos < self.src.len() {
            let start = self.pos;
            let (class, len) = self.classify(start);
            match class {
                CharClass::Whitespace => self.eat_whitespace(),
                CharClass::LineComment => self.eat_line_comment(),
                CharClass::BlockComment => self.eat_block_comment(start),
                CharClass::IdentStart => self.eat_ident(start, len),
                CharClass::Digit => self.eat_number(start),
                CharClass::Quote => self.eat_text(start),
                CharClass::Punct => self.eat_punct(start),
                CharClass::Backtick => self.eat_template(start),
                CharClass::Unknown => self.diagnose_unknown(start, len),
            }
        }
        self.tokens
            .push(Token::new(SyntaxKind::EOF, Span::at(self.pos as u32)));
    }

    /// The scalar beginning at `at` and its UTF-8 length. `at` is always a char
    /// boundary: every lexer advance moves by whole scalars.
    fn scalar_at(&self, at: usize) -> Option<(char, usize)> {
        self.src[at..].chars().next().map(|c| (c, c.len_utf8()))
    }

    /// Classify the scalar at `at`, returning its class and byte length.
    ///
    /// ASCII takes a byte-level fast path; a non-ASCII scalar is decoded and
    /// asked the one identifier question (§4.1). Returning the length is what
    /// keeps `Unknown` from advancing into the middle of a scalar.
    fn classify(&self, at: usize) -> (CharClass, usize) {
        let b = self.bytes()[at];
        if b.is_ascii() {
            let class = match b {
                b' ' | b'\t' | b'\n' | b'\r' => CharClass::Whitespace,
                // A `/` is a comment only when followed by `/` or `*`; otherwise it
                // is punctuation (the division operator / part of a comment opener).
                b'/' if self.starts_with(b"//") => CharClass::LineComment,
                b'/' if self.starts_with(b"/*") => CharClass::BlockComment,
                b'_' | b'a'..=b'z' | b'A'..=b'Z' => CharClass::IdentStart,
                b'0'..=b'9' => CharClass::Digit,
                b'"' => CharClass::Quote,
                b'`' => CharClass::Backtick,
                // Any leading punctuation byte of an operator we recognize. The
                // precise multi-char split happens in `eat_punct`; the class just
                // routes the first byte here.
                b'(' | b')' | b'{' | b'}' | b'[' | b']' | b'<' | b'>' | b'+' | b'-' | b'*'
                | b'/' | b'%' | b'=' | b'!' | b'?' | b':' | b';' | b',' | b'.' | b'|' | b'&'
                | b'#' => CharClass::Punct,
                _ => CharClass::Unknown,
            };
            return (class, 1);
        }
        let (c, len) = self.scalar_at(at).expect("pos is a char boundary");
        let class = if praxis_syntax::ident::is_ident_start(c) {
            CharClass::IdentStart
        } else {
            CharClass::Unknown
        };
        (class, len)
    }

    fn eat_whitespace(&mut self) {
        let start = self.pos;
        while self.pos < self.src.len()
            && matches!(self.bytes()[self.pos], b' ' | b'\t' | b'\n' | b'\r')
        {
            self.pos += 1;
        }
        self.push(SyntaxKind::Whitespace, start);
    }

    fn eat_line_comment(&mut self) {
        let start = self.pos;
        self.pos += 2; // skip leading `//`
        while self.pos < self.src.len() && !matches!(self.bytes()[self.pos], b'\n' | b'\r') {
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

    fn eat_ident(&mut self, start: usize, first_len: usize) {
        // The first scalar is already known to be ident-start; advance past it
        // and consume XID-Continue scalars (§4.1). Advancing by scalar, not by
        // byte, is what keeps a continuation byte from extending the run.
        self.pos += first_len;
        while let Some((c, len)) = self.scalar_at(self.pos) {
            if !praxis_syntax::ident::is_ident_continue(c) {
                break;
            }
            self.pos += len;
        }
        // Look up the keyword table: `let`/`if`/… become their own kinds; the
        // rest stay identifiers. Builtins (`out`, `panic`, type names) are
        // intentionally not keywords.
        let text = &self.src[start..self.pos];
        let kind = SyntaxKind::from_keyword(text).unwrap_or(SyntaxKind::Ident);
        self.push(kind, start);
    }

    /// Lex a numeric literal starting at `start` (the first digit). Recognizes
    /// both integers (`42`) and floats (`3.14`, `2.`, `1e10`, `1.5e-3`).
    ///
    /// A `.` is consumed as part of the literal only when it begins a fraction
    /// — i.e. the byte after the integer part is `.` AND the byte after that is
    /// a digit. This excludes range syntax: `1..5` and `1..=5` lex as `IntLit`
    /// `1` followed by `DOT2` / `DOT2EQ`, never as a malformed float. A trailing
    /// dot with no following digit (`2.`) is a valid float iff the integer part
    /// is nonempty — handled by the `2..` case below still being a range (the
    /// second `.` breaks the fraction).
    ///
    /// A leading-dot float (`.5`) is not reachable here because the dispatch
    /// routes on the first byte: `.` is `Punct`. Leading-dot floats are not
    /// supported (a deliberate simplification); users write `0.5`.
    fn eat_number(&mut self, start: usize) {
        // Integer part: one or more digits (the first is already known present).
        while self.pos < self.src.len() && self.bytes()[self.pos].is_ascii_digit() {
            self.pos += 1;
        }
        let mut is_float = false;
        // Fractional part: `.` followed by a digit. The "followed by a digit"
        // check is what disambiguates `1.5` (float) from `1..5` (range: the
        // next byte is another `.`) and `1.method()` (the next byte is a letter).
        if self.peek_is_dot_then_digit() {
            is_float = true;
            // Consume the `.`.
            self.pos += 1;
            while self.pos < self.src.len() && self.bytes()[self.pos].is_ascii_digit() {
                self.pos += 1;
            }
        }
        // Exponent part: `e` or `E`, optional `+`/`-`, then one or more digits.
        if matches!(self.bytes().get(self.pos), Some(b'e') | Some(b'E')) {
            // Only treat as an exponent if a digit (or signed digit) follows;
            // otherwise `1e` is `IntLit(1)` + `Ident(e)` (a name). `+`/`-` then
            // a digit also counts.
            if self.peek_exponent_has_digits() {
                is_float = true;
                self.pos += 1; // the `e`/`E`
                if matches!(self.bytes().get(self.pos), Some(b'+') | Some(b'-')) {
                    self.pos += 1;
                }
                while self.pos < self.src.len() && self.bytes()[self.pos].is_ascii_digit() {
                    self.pos += 1;
                }
            }
        }
        let kind = if is_float {
            SyntaxKind::FloatLit
        } else {
            SyntaxKind::IntLit
        };
        self.push(kind, start);
    }

    /// True iff the current position is a `.` immediately followed by an ASCII
    /// digit. Used to decide whether the `.` starts a float fraction.
    fn peek_is_dot_then_digit(&self) -> bool {
        matches!(self.bytes().get(self.pos), Some(b'.'))
            && matches!(self.bytes().get(self.pos + 1), Some(d) if d.is_ascii_digit())
    }

    /// True iff the current position is an `e`/`E` that begins a valid exponent:
    /// `e`/`E` then (optionally `+`/`-`) then at least one digit.
    fn peek_exponent_has_digits(&self) -> bool {
        if !matches!(self.bytes().get(self.pos), Some(b'e') | Some(b'E')) {
            return false;
        }
        let mut i = self.pos + 1;
        if matches!(self.bytes().get(i), Some(b'+') | Some(b'-')) {
            i += 1;
        }
        matches!(self.bytes().get(i), Some(d) if d.is_ascii_digit())
    }

    fn eat_text(&mut self, start: usize) {
        // Double-quoted text literal with `\` escapes (§4). The body is scanned
        // here only to find the closing quote; its semantic value is decoded
        // later. We do validate escapes so a stray trailing backslash is caught.
        self.pos += 1; // opening `"`
        while self.pos < self.src.len() {
            match self.bytes()[self.pos] {
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
                    let esc = self.bytes()[self.pos + 1];
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
            .unwrap_or_else(|| single_punct(self.bytes()[start]).unwrap_or(SyntaxKind::ERROR));
        self.push(kind, start);
    }

    /// Try to match the longest operator beginning at `pos`, advancing `pos`
    /// past it. Returns the matched kind for multi-char operators, or `None`
    /// for a bare single-byte punctuation byte (the caller then falls back to
    /// [`single_punct`]).
    fn match_op(&mut self) -> Option<SyntaxKind> {
        // Three-char operators first (only `..=` so far), then two-char. Order
        // matters: longest first so `..=` is not misread as `..` then `=`.
        let three = self.bytes().get(self.pos..self.pos + 3);
        if let Some([b'.', b'.', b'=']) = three {
            self.pos += 3;
            return Some(SyntaxKind::DOT2EQ);
        }
        let two = self.bytes().get(self.pos..self.pos + 2);
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
        while self.pos < self.src.len() && self.bytes()[self.pos] != b'`' {
            // Honour `\\` so an escaped backtick doesn't terminate the template.
            if self.bytes()[self.pos] == b'\\' && self.pos + 1 < self.src.len() {
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

    fn diagnose_unknown(&mut self, start: usize, len: usize) {
        // Advance one whole scalar so we make progress without splitting a
        // multi-byte character into several "unexpected character" diagnostics.
        self.pos += len;
        // Emit an ERROR token covering it. The tree is lossless (ADR-003): a
        // character the lexer cannot classify is still source text, and
        // dropping it silently means the tree no longer reproduces the file.
        self.push(SyntaxKind::ERROR, start);
        let span = Span::new(start as u32, self.pos as u32);
        self.diagnostic(
            span,
            DiagnosticCode::new(DiagnosticCategory::Lex, 3),
            "unexpected character in source",
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

    #[inline]
    fn bytes(&self) -> &'a [u8] {
        self.src.as_bytes()
    }

    fn starts_with(&self, needle: &[u8]) -> bool {
        self.bytes()[self.pos..].starts_with(needle)
    }
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
enum CharClass {
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
        // The `@` becomes an ERROR token and lexing continues. It must not be
        // dropped: the tree is lossless (ADR-003), so every byte of the source
        // has to be reachable through some token — this test previously
        // asserted the opposite, contradicting both ADR-003 and the doc on
        // `SyntaxKind::ERROR`.
        assert!(kinds.contains(&SyntaxKind::ERROR));
        assert!(kinds.contains(&SyntaxKind::KW_LET));
        assert!(kinds.contains(&SyntaxKind::IntLit));
    }

    /// Every byte of the input is covered by exactly one token, in order —
    /// including bytes the lexer cannot classify.
    #[test]
    fn tokens_tile_the_source_even_across_unknown_characters() {
        let src = "let x = 1 @ \u{2192} 2";
        let out = lex(FileId::SYNTHETIC, src);
        let mut at = 0usize;
        for token in &out.tokens {
            assert_eq!(
                token.span.start().to_usize(),
                at,
                "gap before {:?}",
                token.kind
            );
            at = token.span.end().to_usize();
        }
        assert_eq!(at, src.len(), "tokens do not cover the source");
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
error[T003]: unexpected character in source

  day.px:1:4
  1 | let @ = 1
    |     ^ unexpected character in source
");
    }

    // ---- New M1 coverage ----

    #[test]
    fn keywords_split_from_identifiers() {
        let (kinds, diags) = lex_text(
            "let var fn if else while for in loop match return break continue read struct enum true false",
        );
        assert!(diags.is_empty());
        for keyword in [
            SyntaxKind::KW_LET,
            SyntaxKind::KW_VAR,
            SyntaxKind::KW_FN,
            SyntaxKind::KW_IF,
            SyntaxKind::KW_ELSE,
            SyntaxKind::KW_WHILE,
            SyntaxKind::KW_FOR,
            SyntaxKind::KW_IN,
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
        ] {
            assert!(
                kinds.contains(&keyword),
                "missing keyword token {keyword:?}"
            );
        }
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

    /// The old rule accepted every byte `>= 0x80` as an identifier
    /// continuation, so a non-identifier scalar silently extended the name
    /// instead of ending it.
    #[test]
    fn a_non_identifier_scalar_ends_an_identifier_run() {
        let (kinds, diags) = lex_text("ab\u{2192}cd");
        let meaningful: Vec<_> = kinds
            .into_iter()
            .filter(|kind| !kind.is_trivia() && *kind != SyntaxKind::EOF)
            .collect();
        assert_eq!(
            meaningful,
            vec![SyntaxKind::Ident, SyntaxKind::ERROR, SyntaxKind::Ident],
            "`\u{2192}` is not an identifier character, so it splits the run"
        );
        assert_eq!(diags.len(), 1, "the arrow itself is one bad character");
    }

    #[test]
    fn regression_unicode_identifier_may_start_with_a_unicode_scalar() {
        let (kinds, diags) = lex_text("let λ = 1");
        assert!(diags.is_empty(), "Unicode identifier faulted: {diags:?}");
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == SyntaxKind::Ident)
                .count(),
            1,
            "`λ` should be one identifier token"
        );
    }

    #[test]
    #[ignore = "known bug: a lone `_` is emitted as Ident, making wildcard patterns bindings"]
    fn regression_lone_underscore_has_its_dedicated_token_kind() {
        let (kinds, diags) = lex_text("_");
        assert!(diags.is_empty(), "underscore should lex cleanly: {diags:?}");
        let meaningful: Vec<_> = kinds
            .into_iter()
            .filter(|kind| !kind.is_trivia() && *kind != SyntaxKind::EOF)
            .collect();
        assert_eq!(meaningful, vec![SyntaxKind::UNDERSCORE]);
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

    // ---- Float literal lexing (§4.12) ----

    /// Lex a single numeric token (no trivia) and assert its kind + text span.
    fn lex_one_number(text: &str) -> (SyntaxKind, String) {
        let map = SourceMap::new();
        let id = map.intern("test.px", text);
        let out = lex(id, text);
        let tok = out
            .tokens
            .iter()
            .find(|t| matches!(t.kind, SyntaxKind::IntLit | SyntaxKind::FloatLit))
            .expect("a numeric token");
        let span = tok.span.start().0 as usize..tok.span.end().0 as usize;
        (tok.kind, text[span].to_string())
    }

    #[test]
    fn float_with_fraction() {
        let (kind, text) = lex_one_number("3.14");
        assert_eq!(kind, SyntaxKind::FloatLit);
        assert_eq!(text, "3.14");
    }

    #[test]
    fn float_trailing_dot() {
        // `2.` — a digit, a dot, then EOF. Not a range (no second dot).
        let (kind, text) = lex_one_number("2.");
        assert_eq!(kind, SyntaxKind::IntLit);
        assert_eq!(text, "2");
        // `2.` alone (dot then EOF) does NOT consume the dot as a fraction
        // because there's no digit after it. The dot becomes a separate DOT
        // token — so the number is just `2` (an Int). A trailing-dot float
        // requires `2.0`.
        let map = SourceMap::new();
        let id = map.intern("test.px", "2.");
        let out = lex(id, "2.");
        assert!(out.tokens.iter().any(|t| t.kind == SyntaxKind::DOT));
    }

    #[test]
    fn float_with_exponent() {
        let (kind, text) = lex_one_number("1e10");
        assert_eq!(kind, SyntaxKind::FloatLit);
        assert_eq!(text, "1e10");
    }

    #[test]
    fn float_with_fraction_and_signed_exponent() {
        let (kind, text) = lex_one_number("1.5e-3");
        assert_eq!(kind, SyntaxKind::FloatLit);
        assert_eq!(text, "1.5e-3");
    }

    #[test]
    fn float_with_uppercase_exponent_and_plus() {
        let (kind, text) = lex_one_number("2.0E+5");
        assert_eq!(kind, SyntaxKind::FloatLit);
        assert_eq!(text, "2.0E+5");
    }

    #[test]
    fn range_not_lexed_as_float() {
        // `1..5` must lex as IntLit(1), DOT2, IntLit(5) — never a float.
        let (kinds, diags) = lex_text("1..5");
        assert!(diags.is_empty(), "got diagnostics: {diags:?}");
        assert!(kinds.contains(&SyntaxKind::IntLit));
        assert!(kinds.contains(&SyntaxKind::DOT2));
        assert!(!kinds.contains(&SyntaxKind::FloatLit));
    }

    #[test]
    fn inclusive_range_not_lexed_as_float() {
        // `1..=5` must lex as IntLit(1), DOT2EQ, IntLit(5).
        let (kinds, diags) = lex_text("1..=5");
        assert!(diags.is_empty(), "got diagnostics: {diags:?}");
        assert!(kinds.contains(&SyntaxKind::IntLit));
        assert!(kinds.contains(&SyntaxKind::DOT2EQ));
        assert!(!kinds.contains(&SyntaxKind::FloatLit));
    }

    #[test]
    fn float_then_range_boundary() {
        // `1.5..2.5` — the first float ends at the second dot; then DOT2.
        let (kinds, diags) = lex_text("1.5..2.5");
        assert!(diags.is_empty(), "got diagnostics: {diags:?}");
        assert_eq!(
            kinds.iter().filter(|k| **k == SyntaxKind::FloatLit).count(),
            2,
            "expected two FloatLit tokens"
        );
        assert!(kinds.contains(&SyntaxKind::DOT2));
    }

    #[test]
    fn bare_integer_stays_int() {
        let (kind, text) = lex_one_number("42");
        assert_eq!(kind, SyntaxKind::IntLit);
        assert_eq!(text, "42");
    }

    #[test]
    fn trailing_e_without_digits_stays_int() {
        // `1e` — exponent with no digits is `IntLit(1)` then `Ident(e)`.
        let (kinds, _diags) = lex_text("1e");
        assert!(kinds.contains(&SyntaxKind::IntLit));
        assert!(kinds.contains(&SyntaxKind::Ident));
        assert!(!kinds.contains(&SyntaxKind::FloatLit));
    }

    #[test]
    fn dot_method_call_not_lexed_as_float() {
        // `1.method()` — dot followed by a letter is a DOT + Ident, not a float.
        let (kinds, _diags) = lex_text("1.method()");
        assert!(kinds.contains(&SyntaxKind::IntLit));
        assert!(kinds.contains(&SyntaxKind::DOT));
        assert!(kinds.contains(&SyntaxKind::Ident));
        assert!(!kinds.contains(&SyntaxKind::FloatLit));
    }
}
