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

use praxis_source::{DiagCode, Diagnostic, FileId, Severity, Span};
use praxis_syntax::{SyntaxKind, Token};

use praxis_syntax::template::TemplateEnd;

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
    /// Whether the trivia seen since the last meaningful token contained a line
    /// break. Consumed (and cleared) by the next meaningful token, which is
    /// where the parser reads it from (F8/D8, ADR-049).
    pending_newline: bool,
}

impl<'a> Lexer<'a> {
    fn new(file: FileId, text: &'a str) -> Lexer<'a> {
        Lexer {
            file,
            src: text,
            pos: 0,
            tokens: Vec::new(),
            diagnostics: Vec::new(),
            pending_newline: false,
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
        self.push(SyntaxKind::EOF, self.pos);
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
                DiagCode::UnterminatedBlockComment,
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
        //
        // A **lone** `_` is neither (FE-02). `is_ident_start` accepts it — and
        // must, because `_x` and `snake_case` are identifiers — so a bare
        // underscore used to arrive downstream as an ordinary `Ident`, which
        // made every wildcard a *binding* named `_`: two `_` arms of one match
        // were a duplicate declaration, and `Point { x: 1, _: 2 }` named a
        // field. `_` followed by anything ident-continue is still an
        // identifier; only the one-character run is the wildcard.
        let text = &self.src[start..self.pos];
        let kind = if text == "_" {
            SyntaxKind::UNDERSCORE
        } else {
            SyntaxKind::from_keyword(text).unwrap_or(SyntaxKind::Ident)
        };
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
    ///
    /// Every digit run admits `_` separators between its digits (REP-11), so
    /// `1_000`, `3.141_592` and `1e1_0` are each one token. The rule is
    /// `praxis_syntax::numeric`'s, and the same module strips them back out when
    /// lowering reads the value — the two halves used to disagree, with the
    /// lexer rejecting what the decoder stripped.
    fn eat_number(&mut self, start: usize) {
        // Integer part: one or more digits (the first is already known present).
        self.eat_digit_run();
        let mut is_float = false;
        // Fractional part: `.` followed by a digit. The "followed by a digit"
        // check is what disambiguates `1.5` (float) from `1..5` (range: the
        // next byte is another `.`) and `1.method()` (the next byte is a letter).
        // A `_` cannot open a fraction for the same reason: `1._0` is not a
        // float, so a separator is only ever *between* digits.
        //
        // …and a digit run that *itself* follows a `.` is a **tuple index**, so
        // it takes no fraction at all (REP-08). `t.0.1` is `t`, `.0`, `.1` — two
        // indices — and lexing the `0.1` as one float is what made a nested tuple
        // unreadable even once the parser accepted `p.0`. The rule is adjacency:
        // the immediately preceding token, with no trivia between, is a `DOT`.
        // A `..` is `DOT2` and is a different token, so `0..1.5` is untouched,
        // and no float literal in any program has a bare `DOT` before it — in
        // `1.5` the `.` is consumed *inside* this function and never emitted.
        if !self.preceded_by_dot(start) && self.peek_is_dot_then_digit() {
            is_float = true;
            // Consume the `.`.
            self.pos += 1;
            self.eat_digit_run();
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
                self.eat_digit_run();
            }
        }
        let kind = if is_float {
            SyntaxKind::FloatLit
        } else {
            SyntaxKind::IntLit
        };
        self.push(kind, start);
    }

    /// Consume digits and the `_` separators between them, leaving `pos` on the
    /// first byte that is neither (REP-11).
    ///
    /// Each caller has already consumed at least one digit, which is what makes
    /// a separator's left-hand digit certain; `separator_run_len` checks it
    /// regardless. A trailing `_` is not consumed — `1_` is the literal `1`
    /// followed by the `_` token (FE-02), not a literal with a dangling
    /// separator.
    fn eat_digit_run(&mut self) {
        loop {
            while self.pos < self.src.len() && self.bytes()[self.pos].is_ascii_digit() {
                self.pos += 1;
            }
            let run = praxis_syntax::numeric::separator_run_len(self.bytes(), self.pos);
            if run == 0 {
                return;
            }
            self.pos += run;
        }
    }

    /// True iff the token just emitted is a bare `DOT` ending exactly at `start`
    /// — nothing between it and the literal now being lexed, not even whitespace
    /// (REP-08).
    ///
    /// The one caller is [`Self::eat_number`]: a digit run in that position is a
    /// **tuple index** and takes no fractional part, so `t.0.1` is two indices
    /// rather than an index and a float.
    ///
    /// It has to be the *token* and not the source byte. In `1.5..2.5` the byte
    /// before `2` is a `.` too — the second one of the `..` — but that `.` was
    /// consumed into a `DOT2`, which is a different token and leaves `2.5` the
    /// float it is.
    fn preceded_by_dot(&self, start: usize) -> bool {
        self.tokens.last().is_some_and(|last| {
            last.kind == SyntaxKind::DOT && last.span.end().to_u32() as usize == start
        })
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
                            DiagCode::InvalidEscape,
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
            DiagCode::UnterminatedTextLiteral,
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
            Some(b"&&") => Some(SyntaxKind::AMP2),
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

    /// Consume a backtick template as **one** token, interior and all.
    ///
    /// The interior is opaque here; `praxis_input_parser::scan_template`
    /// re-scans it. What is not opaque is where the token *ends*, and that is a
    /// consequence of D10: a capture body is a full parser expression, so
    /// `` `{g:choice(A: `{x:int}`)}` `` is one template containing another. The
    /// predecessor closed at the first unescaped backtick, which cut that into
    /// three unrelated token runs and produced errors about the fragments.
    ///
    /// **The rule is not written here.** It lives in
    /// [`praxis_syntax::template`], because the scanner that re-reads this
    /// token's interior has to find the same nested templates and the same
    /// closing backtick inside it. When this function had its own copy of the
    /// rule the two disagreed at once: this one counted `{`/`}` inside string
    /// literals, so `` `{c:one_of("{")}` `` — which the scanner accepted —
    /// closed nowhere and swallowed the rest of the file.
    fn eat_template(&mut self, start: usize) {
        match praxis_syntax::template::template_end(self.src, start) {
            TemplateEnd::Closed(end) => self.pos = end,
            TemplateEnd::Unterminated => {
                self.pos = self.src.len();
                self.diagnostic(
                    Span::new(start as u32, self.pos as u32),
                    DiagCode::UnterminatedTemplate,
                    "unterminated backtick template",
                );
            }
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
            DiagCode::UnexpectedCharacter,
            "unexpected character in source",
        );
    }

    // --- helpers ---

    /// Emit a token covering `start..pos`, threading the newline fact through
    /// it (F8/D8, ADR-049).
    ///
    /// Trivia *accumulates* the fact — a line break anywhere in the run before a
    /// meaningful token counts, so `1 /* \n */ + 2` and `1\n+ 2` agree. A
    /// meaningful token *consumes* it: it carries the flag and clears the
    /// pending state, so only the first token on a line reports one.
    fn push(&mut self, kind: SyntaxKind, start: usize) {
        let preceded_by_newline = if kind.is_trivia() {
            // A line comment runs to the end of its line, so reaching its end is
            // reaching a line break even when EOF eats the `\n` itself.
            self.pending_newline |=
                kind == SyntaxKind::LineComment || self.src[start..self.pos].contains(['\n', '\r']);
            self.pending_newline
        } else {
            std::mem::take(&mut self.pending_newline)
        };
        self.tokens.push(Token::new(
            kind,
            Span::new(start as u32, self.pos as u32),
            preceded_by_newline,
        ));
    }

    fn diagnostic(&mut self, span: Span, code: DiagCode, message: &str) {
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
        assert_eq!(diags[0].kind(), DiagCode::UnexpectedCharacter);
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
        assert_eq!(diags[0].kind(), DiagCode::UnterminatedBlockComment);
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
        assert_eq!(diags[0].kind(), DiagCode::UnterminatedTemplate);
    }

    /// **D10's lexer half.** A capture body is a full parser expression, so a
    /// template may contain a template. Closing at the first unescaped backtick
    /// cut `` `{g:choice(A: `{x:int}`)}` `` into three unrelated token runs, and
    /// the scanner never saw the template the source wrote.
    ///
    /// A backtick closes only at brace depth 0; inside a capture it opens a
    /// nested run.
    #[test]
    fn a_nested_backtick_template_is_one_token() {
        let src = "let p = `{g:choice(A: `{x:int}`, B: word)}`";
        let (kinds, diags) = lex_text(src);
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(
            kinds
                .iter()
                .filter(|k| **k == SyntaxKind::BacktickTemplate)
                .count(),
            1,
            "the whole thing is one template token, inner backticks included"
        );

        // Two levels deep, and two nested templates side by side.
        for src in [
            "let p = `{a:choice(A: `{b:choice(C: `{c:int}`)}`)}`",
            "let p = `{a:choice(A: `{x:int}`, B: `{y:word}`)}`",
        ] {
            let (kinds, diags) = lex_text(src);
            assert!(diags.is_empty(), "{src}: {diags:?}");
            assert_eq!(
                kinds
                    .iter()
                    .filter(|k| **k == SyntaxKind::BacktickTemplate)
                    .count(),
                1,
                "{src}"
            );
        }

        // An escaped backtick still cannot terminate anything, at either depth.
        let (_, diags) = lex_text(r"let p = `a\`b`");
        assert!(diags.is_empty(), "{diags:?}");

        // And an outer template that never closes still faults.
        let (_, diags) = lex_text("let p = `{g:choice(A: `{x:int}`)}");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].kind(), DiagCode::UnterminatedTemplate);
    }

    /// A brace inside a **string literal** is text, not structure.
    ///
    /// This is the regression the shared rule exists to prevent: the lexer's
    /// own copy of the brace counter had no string arm, so `one_of("{")` — a
    /// legal §7.5 program the input parser's scanner accepts — left the counter
    /// above zero at the closing backtick, which then read as an *opener* and
    /// swallowed the rest of the file into one token plus a false `T002`.
    #[test]
    fn a_brace_inside_a_string_does_not_extend_the_template() {
        for template in [
            r#"`{c:one_of("{")}`"#,
            r#"`{c:one_of("}")}`"#,
            r#"`{s:sep("{", int)}`"#,
            r#"`{c:one_of("`")}`"#,
        ] {
            let src = format!("let p = {template}\nlet q = 1\n");
            let out = lex(FileId::SYNTHETIC, &src);
            assert!(
                out.diagnostics.is_empty(),
                "{template}: {:?}",
                out.diagnostics
            );
            let templates: Vec<&Token> = out
                .tokens
                .iter()
                .filter(|t| t.kind == SyntaxKind::BacktickTemplate)
                .collect();
            assert_eq!(templates.len(), 1, "{template}");
            assert_eq!(
                &src[templates[0].span.start().to_usize()..templates[0].span.end().to_usize()],
                template,
                "the token is the template and nothing after it"
            );
        }
    }

    /// A lexer walks whatever the file contains, so nesting is bounded (D10) —
    /// and the bound is *exactly* [`praxis_syntax::MAX_TEMPLATE_NESTING`].
    ///
    /// The predecessor of this test fed 5,000 unclosed openers and asserted
    /// only that `UnterminatedTemplate` was reported somewhere, which the old
    /// lexer — which had no nesting at all — also did. It discriminated
    /// nothing. What only a bounded, nesting lexer produces is this: a properly
    /// closed nest of `MAX_TEMPLATE_NESTING` templates is **one** token, and
    /// one level deeper is not.
    #[test]
    fn template_nesting_is_bounded_at_exactly_max_template_nesting() {
        use praxis_syntax::MAX_TEMPLATE_NESTING;

        // `n` nested templates whose innermost holds a lone `"` as literal
        // text. That quote is text only if the innermost template really is
        // entered as a template, at capture depth 0; if the bound stopped one
        // level short the same byte sits inside the parent's capture, where a
        // quote opens a string literal that never closes.
        fn nested(n: usize) -> String {
            let mut s = String::new();
            for _ in 0..n - 1 {
                s.push_str("`{a:");
            }
            s.push_str("`\"`");
            for _ in 0..n - 1 {
                s.push_str("}`");
            }
            s
        }

        let at_the_bound = nested(MAX_TEMPLATE_NESTING);
        let (kinds, diags) = lex_text(&format!("let p = {at_the_bound}\nlet q = 1\n"));
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(
            kinds
                .iter()
                .filter(|k| **k == SyntaxKind::BacktickTemplate)
                .count(),
            1,
            "a nest exactly at the bound is one token"
        );

        let past = nested(MAX_TEMPLATE_NESTING + 1);
        let (_, diags) = lex_text(&format!("let p = {past}\nlet q = 1\n"));
        assert!(
            diags
                .iter()
                .any(|d| d.kind() == DiagCode::UnterminatedTemplate),
            "one level past the bound the innermost template is not entered — an \
             unbounded lexer reports nothing here"
        );

        // And the pathological case reports rather than overflowing the stack.
        let (_, diags) = lex_text(&format!("let p = {}", "`{a:".repeat(5_000)));
        assert!(
            diags
                .iter()
                .any(|d| d.kind() == DiagCode::UnterminatedTemplate),
            "deep nesting must report, not overflow"
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
    fn regression_lone_underscore_has_its_dedicated_token_kind() {
        let (kinds, diags) = lex_text("_");
        assert!(diags.is_empty(), "underscore should lex cleanly: {diags:?}");
        let meaningful: Vec<_> = kinds
            .into_iter()
            .filter(|kind| !kind.is_trivia() && *kind != SyntaxKind::EOF)
            .collect();
        assert_eq!(meaningful, vec![SyntaxKind::UNDERSCORE]);
    }

    /// …and only the lone one. An underscore is a legal identifier *character*
    /// (§4.1), so the split has to be on the whole run, not on the first byte.
    #[test]
    fn an_underscore_inside_a_name_is_still_an_identifier() {
        let (kinds, diags) = lex_text("_x __ x_ _1 snake_case");
        assert!(diags.is_empty(), "clean lex: {diags:?}");
        let meaningful: Vec<_> = kinds
            .into_iter()
            .filter(|kind| !kind.is_trivia() && *kind != SyntaxKind::EOF)
            .collect();
        assert_eq!(meaningful, vec![SyntaxKind::Ident; 5]);
    }

    // --- F8: the newline fact the parser needs (D8, ADR-049) ----------------

    /// `(kind, preceded_by_newline)` for the meaningful tokens, EOF included.
    fn newline_flags(text: &str) -> Vec<(SyntaxKind, bool)> {
        let map = SourceMap::new();
        let id = map.intern("test.px", text);
        lex(id, text)
            .tokens
            .into_iter()
            .filter(|t| !t.kind.is_trivia())
            .map(|t| (t.kind, t.preceded_by_newline))
            .collect()
    }

    #[test]
    fn only_the_first_token_on_a_line_is_preceded_by_a_newline() {
        use SyntaxKind::*;
        assert_eq!(
            newline_flags("let a\nlet b"),
            vec![
                (KW_LET, false),
                (Ident, false),
                (KW_LET, true),
                (Ident, false),
                (EOF, false),
            ]
        );
    }

    /// Two statements on one line report no line break anywhere — which is
    /// exactly what FE-04's separator check has to be able to see.
    #[test]
    fn same_line_tokens_report_no_newline() {
        assert!(newline_flags("let a = 1 let b = 2")
            .iter()
            .all(|(_, newline)| !newline));
    }

    /// The fact belongs to the whole trivia run, not to its last token: a
    /// comment after the break must not hide it.
    #[test]
    fn a_line_break_anywhere_in_the_trivia_run_counts() {
        let flags = newline_flags("let a\n  // why\n  let b");
        assert_eq!(flags[2], (SyntaxKind::KW_LET, true));

        let commented = newline_flags("1 /* over\ntwo lines */ + 2");
        assert_eq!(commented[1], (SyntaxKind::PLUS, true));
    }

    /// A line comment ends its line by construction, so it reads as a break even
    /// when EOF eats the `\n` that would otherwise follow.
    #[test]
    fn a_line_comment_ends_the_line_it_is_on() {
        let flags = newline_flags("let a // trailing");
        assert_eq!(flags.last().copied(), Some((SyntaxKind::EOF, true)));
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
        assert_eq!(diags[0].kind(), DiagCode::UnterminatedTextLiteral);
    }

    #[test]
    fn invalid_escape_faults() {
        let (_, diags) = lex_text("\"bad \\q escape\"");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].kind(), DiagCode::InvalidEscape);
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

    /// REP-11: a `_` between digits belongs to the literal, in every digit run.
    ///
    /// `1_000` was a `P002` at the `_` and `9_223_372_036_854_775_808` lexed as
    /// `9` followed by an identifier — while lowering stripped separators on the
    /// other side, so the pair never met. The token text keeps the separators;
    /// removing them is the decoder's half.
    #[test]
    fn a_digit_separator_belongs_to_the_literal() {
        for (src, kind) in [
            ("1_000", SyntaxKind::IntLit),
            ("1_0_0", SyntaxKind::IntLit),
            // A run is one separator: nothing here counts `_`s.
            ("1__0", SyntaxKind::IntLit),
            // The finding's own case, which used to lex as `9` + an identifier.
            ("9_223_372_036_854_775_808", SyntaxKind::IntLit),
            // Every digit run, not just the integer part.
            ("3.141_592", SyntaxKind::FloatLit),
            ("1_0.5", SyntaxKind::FloatLit),
            ("1e1_0", SyntaxKind::FloatLit),
            ("1.5e-1_0", SyntaxKind::FloatLit),
        ] {
            let (got, text) = lex_one_number(src);
            assert_eq!(got, kind, "{src} lexed as {got:?}");
            assert_eq!(text, src, "{src} did not lex as one token");
            let (_, diags) = lex_text(src);
            assert!(diags.is_empty(), "{src} reported {diags:?}");
        }
    }

    /// …and a `_` with no digit after it is not part of the literal at all.
    ///
    /// `1_` is the literal `1` followed by the `_` token FE-02 gave a kind of
    /// its own — which the parser rejects where it stands. A separator has
    /// digits on *both* sides, so the number never ends in punctuation.
    #[test]
    fn a_trailing_separator_is_not_part_of_the_literal() {
        let (kind, text) = lex_one_number("1_");
        assert_eq!(kind, SyntaxKind::IntLit);
        assert_eq!(text, "1");
        let (kinds, _) = lex_text("1_");
        assert!(kinds.contains(&SyntaxKind::UNDERSCORE));

        // A name after the `_` is a name: `1_a` is `1` then `_a`.
        let (kind, text) = lex_one_number("1_a");
        assert_eq!(kind, SyntaxKind::IntLit);
        assert_eq!(text, "1");
        let (kinds, _) = lex_text("1_a");
        assert!(kinds.contains(&SyntaxKind::Ident));

        // A separator cannot open a fraction — `1._0` is not a float.
        let (kind, text) = lex_one_number("1._0");
        assert_eq!(kind, SyntaxKind::IntLit);
        assert_eq!(text, "1");
        let (kinds, _) = lex_text("1._0");
        assert!(!kinds.contains(&SyntaxKind::FloatLit));
    }

    /// **REP-08.** A digit run immediately after a `.` is a **tuple index**, so
    /// it takes no fractional part: `t.0.1` is two indices, not an index and the
    /// float `0.1`.
    ///
    /// The parser accepting `p.0` is not enough on its own — a nested tuple was
    /// unreadable while the lexer folded `0.1` into one `FloatLit`. The rule is
    /// adjacency to a bare `DOT` **token**, which is why `1.5..2.5` is untouched:
    /// the byte before its `2` is a `.` too, but that one was consumed into a
    /// `DOT2`.
    #[test]
    fn a_digit_run_after_a_dot_is_an_index_and_takes_no_fraction() {
        // The defect's own case.
        let (kinds, _) = lex_text("t.0.1");
        assert!(
            !kinds.contains(&SyntaxKind::FloatLit),
            "`t.0.1` is two indices: {kinds:?}"
        );
        assert_eq!(
            kinds.iter().filter(|k| **k == SyntaxKind::IntLit).count(),
            2
        );
        assert_eq!(kinds.iter().filter(|k| **k == SyntaxKind::DOT).count(), 2);

        // A single index, and a wider one.
        for src in ["p.0", "x.10", "p.0 + 1"] {
            let (kinds, diags) = lex_text(src);
            assert!(diags.is_empty(), "{src}: {diags:?}");
            assert!(kinds.contains(&SyntaxKind::IntLit), "{src}");
            assert!(!kinds.contains(&SyntaxKind::FloatLit), "{src}");
        }

        // …and every float that is *not* in that position still lexes as one.
        for src in ["3.0", "1.5", "let x = 0.25", "1.5e3", "3.141_592"] {
            let (kinds, diags) = lex_text(src);
            assert!(diags.is_empty(), "{src}: {diags:?}");
            assert!(kinds.contains(&SyntaxKind::FloatLit), "{src}: {kinds:?}");
        }
        // The one that made the rule a *token* rule: the byte before `2` is the
        // second `.` of the `..`, but that `.` is inside a `DOT2`.
        let (kinds, _) = lex_text("1.5..2.5");
        assert_eq!(
            kinds.iter().filter(|k| **k == SyntaxKind::FloatLit).count(),
            2,
            "both bounds are floats: {kinds:?}"
        );
        assert!(kinds.contains(&SyntaxKind::DOT2));
        // A range whose upper bound is a float, with an integer lower bound —
        // the same trap one token earlier.
        let (kinds, _) = lex_text("0..1.5");
        assert!(kinds.contains(&SyntaxKind::FloatLit), "{kinds:?}");
        // A method call on a float literal is unaffected: the `.` before `sqrt`
        // is not before a digit.
        let (kinds, _) = lex_text("1.5.sqrt()");
        assert!(kinds.contains(&SyntaxKind::FloatLit), "{kinds:?}");
    }

    /// A separated bound is still a bound: `1_000..2_000` is a range (TY-34),
    /// not a float and not one token.
    #[test]
    fn a_separated_literal_is_still_a_range_bound() {
        let (kinds, diags) = lex_text("1_000..2_000");
        assert!(diags.is_empty(), "got diagnostics: {diags:?}");
        assert_eq!(
            kinds.iter().filter(|k| **k == SyntaxKind::IntLit).count(),
            2,
            "expected two IntLit tokens"
        );
        assert!(kinds.contains(&SyntaxKind::DOT2));
        assert!(!kinds.contains(&SyntaxKind::FloatLit));
    }
}
