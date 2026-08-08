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
//! - `T005` — invalid escape in a text or character literal.
//! - `T006` — unterminated character literal.
//! - `T007` — a character literal that is not exactly one character.

use praxis_source::{DiagCode, Diagnostic, FileId, Severity, Span};
use praxis_syntax::{SyntaxKind, Token};

use praxis_syntax::interp::{FragmentEnd, TextEnd};
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
/// This is the stable front-end entry point the CLI and parser both call.
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
    /// where the parser reads it from (ADR-049).
    pending_newline: bool,
    /// The open interpolation holes, innermost last, each holding the brace
    /// depth **within** it (§8.1, ADR-147).
    ///
    /// This is the lexer's only mode stack, and the one question it answers is
    /// what a `}` is: at depth 0 of the innermost hole it closes the hole and
    /// literal text resumes, and anywhere else it is the ordinary `R_BRACE` that
    /// closes a block, a record literal or a set. `"{if x { 1 } else { 2 }}"`
    /// needs exactly that and nothing more.
    ///
    /// A frame is pushed only by a fragment token, and a fragment token is only
    /// emitted for a literal [`praxis_syntax::interp::text_end`] has already
    /// proved closes on its line. So there is no newline, no EOF and no
    /// malformed literal that can leave a frame on it (ADR-147 decision 5).
    holes: Vec<u32>,
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
            holes: Vec::new(),
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
                CharClass::SingleQuote => self.eat_char(start),
                CharClass::Punct => self.eat_punct_tracking_holes(start),
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
                // A `'` opens a character literal and nothing else — it is not
                // a lifetime sigil, not a digit separator and not part of an
                // identifier (ADR-141).
                b'\'' => CharClass::SingleQuote,
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
        // Look up the keyword table: `var`/`if`/… become their own kinds; the
        // rest stay identifiers. Builtins (`out`, `panic`, type names) are
        // intentionally not keywords.
        //
        // A **lone** `_` is neither: it gets its own `UNDERSCORE` kind, so the
        // wildcard is never a *binding* named `_` (two `_` arms of one match
        // would be a duplicate declaration, and `Point { x: 1, _: 2 }` would
        // name a field). `is_ident_start` accepts `_` — and must, because `_x`
        // and `snake_case` are identifiers — so the split is on the whole run:
        // `_` followed by anything ident-continue is still an identifier.
        let text = &self.src[start..self.pos];
        let kind = if text == "_" {
            SyntaxKind::UNDERSCORE
        } else {
            SyntaxKind::from_keyword(text).unwrap_or(SyntaxKind::Ident)
        };
        self.push(kind, start);
    }

    /// Lex a numeric literal starting at `start` (the first digit). Recognizes
    /// both integers (`42`) and floats (`3.14`, `1e10`, `1.5e-3`).
    ///
    /// A `.` is consumed as part of the literal only when it begins a fraction
    /// — i.e. the byte after the integer part is `.` AND the byte after that is
    /// a digit. This excludes range syntax: `1..5` and `1..=5` lex as `IntLit`
    /// `1` followed by `DOT2` / `DOT2EQ`, never as a malformed float.
    ///
    /// **A trailing dot is not part of the literal.** `2.` lexes as `IntLit` `2`
    /// followed by `DOT`, so `var x = 2.` parses as a method call on `2` with no
    /// method name and reports. The float spellings are `2.0` and `2e0`.
    ///
    /// A leading-dot float (`.5`) is not reachable here because the dispatch
    /// routes on the first byte: `.` is `Punct`. Leading-dot floats are not
    /// supported (a deliberate simplification); users write `0.5`.
    ///
    /// Every digit run admits `_` separators between its digits, so `1_000`,
    /// `3.141_592` and `1e1_0` are each one token. The rule is
    /// `praxis_syntax::numeric`'s, and the same module strips them back out when
    /// lowering reads the value, so the two halves cannot disagree.
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
        // it takes no fraction at all: `t.0.1` is `t`, `.0`, `.1` — two indices,
        // not an index and the float `0.1`. The rule is adjacency: the
        // immediately preceding token, with no trivia between, is a `DOT`.
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
    /// first byte that is neither.
    ///
    /// Each caller has already consumed at least one digit, which is what makes
    /// a separator's left-hand digit certain; `separator_run_len` checks it
    /// regardless. A trailing `_` is not consumed — `1_` is the literal `1`
    /// followed by the `UNDERSCORE` token, not a literal with a dangling
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
    /// — nothing between it and the literal now being lexed, not even
    /// whitespace.
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

    /// A `"…"` literal, which with interpolation (§8.1, ADR-147) is one of three
    /// token shapes: a whole `TextLit`, or an `InterpOpen`/`InterpMiddle`/
    /// `InterpClose` fragment run.
    ///
    /// The extent is decided **before** anything is emitted, by
    /// [`praxis_syntax::interp::text_end`], and that ordering is the whole of
    /// ADR-147 decision 5. The lexer enters interpolation mode — the
    /// [`holes`](Self::holes) brace-depth stack that decides whether a later `}`
    /// closes a hole or a block — only for a literal already proved to close on
    /// its line with balanced holes. So no newline and no EOF can reach that
    /// stack, and a literal that does *not* close is one `TextLit` plus `T004`.
    ///
    /// Escapes are still validated here (a stray `\q` is `T005`), and still only
    /// over the *literal text*: the escape rule does not apply inside a hole,
    /// which holds ordinary expression tokens.
    fn eat_text(&mut self, start: usize) {
        match praxis_syntax::interp::text_end(self.src, start) {
            TextEnd::Closed {
                end,
                first_hole: None,
            } => {
                self.validate_escapes(start + 1, end - 1);
                self.pos = end;
                self.push(SyntaxKind::TextLit, start);
            }
            TextEnd::Closed {
                end: _,
                first_hole: Some(brace),
            } => {
                // The opening fragment: `"` … `{`. The hole's own tokens are
                // lexed by the main loop, which is what gives every name in it a
                // real range in the lossless tree (ADR-147 decision 1).
                self.validate_escapes(start + 1, brace);
                self.pos = brace + 1;
                self.push(SyntaxKind::InterpOpen, start);
                self.holes.push(0);
            }
            TextEnd::Unterminated { stopped } => {
                self.pos = stopped;
                self.diagnostic(
                    Span::new(start as u32, self.pos as u32),
                    DiagCode::UnterminatedTextLiteral,
                    "unterminated text literal",
                );
                self.push(SyntaxKind::TextLit, start);
            }
        }
    }

    /// Resume literal text after the `}` at `start` closed a hole, emitting the
    /// [`InterpMiddle`] or [`InterpClose`] fragment that follows it.
    ///
    /// [`InterpMiddle`]: SyntaxKind::InterpMiddle
    /// [`InterpClose`]: SyntaxKind::InterpClose
    ///
    /// This reads the *same* rule the pre-scan in [`eat_text`] read, through the
    /// same module — [`praxis_syntax::interp::fragment_end`] — entered in the
    /// middle. One rule with a scanner-local copy is one rule with two answers.
    ///
    /// [`eat_text`]: Self::eat_text
    ///
    /// The `None` arm cannot be reached: this runs only while the `holes` stack
    /// is non-empty, and the stack is only pushed for a literal the pre-scan
    /// proved closes. It is written as the unterminated path anyway rather than
    /// as a panic, because the cost of being wrong about "cannot happen" in a
    /// lexer is a crash on a user's file.
    fn eat_interp_resume(&mut self, start: usize) {
        match praxis_syntax::interp::fragment_end(self.src, start) {
            Some(FragmentEnd::Hole(brace)) => {
                self.validate_escapes(start + 1, brace);
                self.pos = brace + 1;
                self.push(SyntaxKind::InterpMiddle, start);
                self.holes.push(0);
            }
            Some(FragmentEnd::Close(end)) => {
                self.validate_escapes(start + 1, end - 1);
                self.pos = end;
                self.push(SyntaxKind::InterpClose, start);
            }
            None => {
                self.pos = self.src.len();
                self.diagnostic(
                    Span::new(start as u32, self.pos as u32),
                    DiagCode::UnterminatedTextLiteral,
                    "unterminated text literal",
                );
                self.push(SyntaxKind::TextLit, start);
            }
        }
    }

    /// Report `T005` for every unrecognized escape in the literal text spanning
    /// `from..to`.
    ///
    /// Separate from the scan of *where* a literal ends
    /// (`praxis_syntax::interp`): validation is about the characters and not the
    /// extent, and every fragment of an interpolated literal needs it on the
    /// same terms as a whole literal.
    fn validate_escapes(&mut self, from: usize, to: usize) {
        let mut pos = from;
        while pos < to {
            if self.bytes()[pos] != b'\\' {
                pos += 1;
                continue;
            }
            if pos + 1 >= to {
                break;
            }
            // The escaped **scalar**, not the escaped byte. `"\¡"` is an invalid
            // escape either way, but the diagnostic renderer slices the source
            // at the span, so a span ending inside `¡` panics (`lex_never_panics`
            // is the fuzz target that pins this).
            let (esc, esc_len) = self.scalar_at(pos + 1).expect("pos is a char boundary");
            let end = pos + 1 + esc_len;
            if !esc.is_ascii() || !is_valid_escape(esc as u8) {
                self.diagnostic(
                    Span::new(pos as u32, end as u32),
                    DiagCode::InvalidEscape,
                    "invalid escape in text literal",
                );
            }
            pos = end;
        }
    }

    /// Punctuation, with the one extra question an open interpolation hole asks
    /// (§8.1, ADR-147): is this `}` the end of the hole, or an ordinary brace
    /// inside it?
    ///
    /// The brace depth is kept per hole rather than globally, because holes
    /// nest: `"{f("{y}")}"` opens a second hole while the first is still open,
    /// and the inner `}` must close the inner one. Only the innermost frame is
    /// ever consulted, which is what makes that fall out rather than be arranged.
    ///
    /// Every other punctuation byte routes straight through to [`eat_punct`].
    ///
    /// [`eat_punct`]: Self::eat_punct
    fn eat_punct_tracking_holes(&mut self, start: usize) {
        let byte = self.bytes()[start];
        if let Some(depth) = self.holes.last_mut() {
            match byte {
                b'}' if *depth == 0 => {
                    self.holes.pop();
                    self.eat_interp_resume(start);
                    return;
                }
                b'}' => *depth -= 1,
                b'{' => *depth += 1,
                _ => {}
            }
        }
        self.eat_punct(start);
    }

    /// A `'…'` character literal (§4.3, ADR-141): [`eat_text`]'s shape, with the
    /// one-character rule decided here rather than downstream.
    ///
    /// [`eat_text`]: Self::eat_text
    ///
    /// Three differences from a text literal, each of which is a defect if it is
    /// dropped:
    ///
    /// - the body advances by whole **scalars**, so `'é'` is one character and
    ///   not the two bytes it is written in;
    /// - `\'` is an escape here and `\"` is there — the shared table
    ///   ([`praxis_syntax::literal::decode_escape`]) supplies the rest, so the
    ///   two spellings of `\n` cannot drift;
    /// - a *closed* literal is decoded immediately and its length checked, so
    ///   `'ab'` is `T007` where `"ab"[0]` is a well-typed program that quietly
    ///   means `a`, and `''` is `T007` where `""[0]` is an index fault at run
    ///   time.
    ///
    /// The token is pushed on every path, including the unterminated one, so the
    /// tokens still tile the source (ADR-003) and the parser still sees a
    /// literal to build a node from rather than a hole.
    fn eat_char(&mut self, start: usize) {
        self.pos += 1; // opening `'`
        while self.pos < self.src.len() {
            match self.bytes()[self.pos] {
                b'\'' => {
                    self.pos += 1; // closing quote
                    self.finish_char(start);
                    return;
                }
                b'\\' => {
                    // Need at least one more scalar for the escape.
                    let esc_at = self.pos + 1;
                    if esc_at >= self.src.len() {
                        break;
                    }
                    // The escaped **scalar**, not the escaped byte. `'\¡'` is
                    // an invalid escape either way, but stepping two bytes past
                    // it leaves the cursor inside `¡` — and every later read,
                    // including the slice `finish_char` decodes, then panics on
                    // a char boundary.
                    let (esc, esc_len) = self.scalar_at(esc_at).expect("pos is a char boundary");
                    let bad_at = self.pos;
                    self.pos = esc_at + esc_len;
                    if !is_valid_char_escape(esc) {
                        self.diagnostic(
                            Span::new(bad_at as u32, self.pos as u32),
                            DiagCode::InvalidEscape,
                            "invalid escape in character literal",
                        );
                    }
                }
                b'\n' | b'\r' => {
                    // A character literal ends at its line, for the text
                    // literal's reason: a missing `'` should report where it was
                    // wanted, not swallow the rest of the file.
                    break;
                }
                // A whole scalar, never a byte. `pos += 1` here would put the
                // cursor inside `é`, and the length check below would then count
                // two characters in a literal that names one.
                _ => {
                    let (_, len) = self.scalar_at(self.pos).expect("pos is a char boundary");
                    self.pos += len;
                }
            }
        }
        self.diagnostic(
            Span::new(start as u32, self.pos as u32),
            DiagCode::UnterminatedCharLiteral,
            "unterminated character literal",
        );
        self.push(SyntaxKind::CharLit, start);
    }

    /// Push a closed `'…'` token, reporting a body that does not name exactly
    /// one character.
    ///
    /// The decode is `praxis-syntax`'s, not a second count written here: the
    /// lexer's question ("how many characters is this") and the lowerer's
    /// ("which character is this") have to be the same walk, or `'\n'` is one
    /// character to one of them and two to the other.
    fn finish_char(&mut self, start: usize) {
        use praxis_syntax::literal::CharLitError;

        let raw = &self.src[start..self.pos];
        match praxis_syntax::literal::decode_char_literal(raw).err() {
            None => {}
            Some(CharLitError::Empty) => self.diagnostic(
                Span::new(start as u32, self.pos as u32),
                DiagCode::CharLiteralIsNotOneCharacter,
                "empty character literal: `''` names no character",
            ),
            Some(CharLitError::TooLong) => {
                // The fix is mechanical and it is the one the author probably
                // meant, so it rides as a replacement rather than a `help:`
                // (ADR-132): `'ab'` was almost certainly a `"ab"`.
                let span = Span::new(start as u32, self.pos as u32);
                let body = &raw[1..raw.len() - 1];
                let diag = Diagnostic::new(
                    Severity::Error,
                    DiagCode::CharLiteralIsNotOneCharacter,
                    "a character literal holds exactly one character",
                    praxis_source::FileSpan::new(self.file, span),
                )
                .with_suggestion(
                    praxis_source::FileSpan::new(self.file, span),
                    format!("\"{body}\""),
                    "write it as a text literal",
                );
                self.diagnostics.push(diag);
            }
            // Not reachable from here — this function is only called on a
            // quote the scan itself matched, and the one body that decodes as
            // unterminated (a lone trailing `\`) is exactly the one whose
            // escape ate that quote, so the scan ran to EOF instead. The arm
            // exists because `CharLitError` is the decoder's answer and not
            // this call site's, and a decoder that grows a fourth reason should
            // report it rather than fall into `Empty`'s message.
            Some(CharLitError::Unterminated) => self.diagnostic(
                Span::new(start as u32, self.pos as u32),
                DiagCode::UnterminatedCharLiteral,
                "unterminated character literal",
            ),
        }
        self.push(SyntaxKind::CharLit, start);
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
    /// re-scans it. What is not opaque is where the token *ends*: a capture body
    /// is a full parser expression, so `` `{g:choice(A: `{x:int}`)}` `` is one
    /// template containing another, and a backtick closes only at brace depth 0.
    ///
    /// **The rule is not written here.** It lives in
    /// [`praxis_syntax::template`], because the scanner that re-reads this
    /// token's interior has to find the same nested templates and the same
    /// closing backtick inside it. A scanner-local copy is how the two come to
    /// disagree — over, for instance, whether a brace inside a string literal
    /// counts, which decides `` `{c:one_of("{")}` ``.
    fn eat_template(&mut self, start: usize) {
        // **A template ends at the line it opens on** (ADR-094), so an
        // unterminated one names its own line instead of swallowing the rest of
        // the file into one token plus a cascade of block- and item-level
        // faults.
        //
        // The two kinds are not cosmetic — see
        // `SyntaxKind::UnterminatedBacktickTemplate` for why the alternative
        // (one kind plus a "does it end in a backtick" test at each consumer)
        // is a defect.
        let kind = match praxis_syntax::template::template_end(self.src, start) {
            TemplateEnd::Closed(end) => {
                self.pos = end;
                SyntaxKind::BacktickTemplate
            }
            TemplateEnd::Unterminated(stopped) => {
                self.pos = stopped;
                self.diagnostic(
                    Span::new(start as u32, self.pos as u32),
                    DiagCode::UnterminatedTemplate,
                    "unterminated backtick template",
                );
                SyntaxKind::UnterminatedBacktickTemplate
            }
        };
        self.push(kind, start);
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
    /// it (ADR-049).
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
///
/// Asked of [`praxis_syntax::literal::decode_escape`] rather than listed again,
/// because a lexer that *accepts* an escape the decoder does not *decode* is two
/// answers to one question. That table includes `\{` and `\}`, the spelling of a
/// literal brace now that `{` opens an interpolation hole (ADR-147).
///
/// The one row that is only here is `` \` ``, which the decoder leaves alone:
/// the lexer accepts it so a backtick inside a text literal does not earn
/// `T005`, and preserving it verbatim is what `unquote_text` does with any
/// escape it does not recognize. The carve-out is deliberately no wider.
fn is_valid_escape(esc: u8) -> bool {
    esc.is_ascii() && (praxis_syntax::literal::decode_escape(esc as char).is_some() || esc == b'`')
}

/// Whether `esc` is a recognized escape character inside a character literal.
///
/// The text literal's set **plus** `\'`, and defined in terms of it rather than
/// listed again: a language with two escape tables has two answers to what `\n`
/// is, which is the drift `praxis_syntax::literal` exists to prevent (ADR-141).
/// There is no `\x` or `\u{…}` here because there is none there.
/// Takes a `char` and not a byte, because a character literal's body is scanned
/// by scalar: `'\¡'` must be refused *and* stepped over whole.
fn is_valid_char_escape(esc: char) -> bool {
    esc.is_ascii() && (is_valid_escape(esc as u8) || esc == '\'')
}

#[derive(Clone, Copy)]
enum CharClass {
    Whitespace,
    LineComment,
    BlockComment,
    IdentStart,
    Digit,
    Quote,
    SingleQuote,
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
        let (kinds, diags) = lex_text("var x = 42 // hi\n");
        assert!(diags.is_empty(), "got diagnostics: {diags:?}");
        assert!(kinds.contains(&SyntaxKind::KW_VAR)); // keyword split out
        assert!(kinds.contains(&SyntaxKind::Ident));
        assert!(kinds.contains(&SyntaxKind::IntLit));
        assert!(kinds.contains(&SyntaxKind::Whitespace));
        assert!(kinds.contains(&SyntaxKind::LineComment));
        assert!(kinds.last().is_some_and(|k| *k == SyntaxKind::EOF));
    }

    #[test]
    fn unknown_byte_emits_one_diagnostic() {
        let (kinds, diags) = lex_text("var @ = 1");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].kind(), DiagCode::UnexpectedCharacter);
        // The `@` becomes an ERROR token and lexing continues. It must not be
        // dropped: the tree is lossless (ADR-003), so every byte of the source
        // has to be reachable through some token.
        assert!(kinds.contains(&SyntaxKind::ERROR));
        assert!(kinds.contains(&SyntaxKind::KW_VAR));
        assert!(kinds.contains(&SyntaxKind::IntLit));
    }

    /// Every byte of the input is covered by exactly one token, in order —
    /// including bytes the lexer cannot classify.
    #[test]
    fn tokens_tile_the_source_even_across_unknown_characters() {
        // A character literal and a multi-byte scalar inside one are in here for
        // `eat_char`'s sake: it is the second scan in the lexer that advances by
        // whole scalars, and a `pos += 1` in it would leave the next token
        // starting mid-scalar.
        let src = "var x = 1 @ \u{2192} 2 'é' ''";
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
        let (kinds, diags) = lex_text("var p = `{x:int}`");
        assert!(diags.is_empty());
        assert!(kinds.contains(&SyntaxKind::BacktickTemplate));
    }

    #[test]
    fn unterminated_template_faults() {
        let (_, diags) = lex_text("var p = `never closes");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].kind(), DiagCode::UnterminatedTemplate);
    }

    /// A capture body is a full parser expression, so a template may contain a
    /// template: `` `{g:choice(A: `{x:int}`)}` `` is **one** token.
    ///
    /// A backtick closes only at brace depth 0; inside a capture it opens a
    /// nested run.
    #[test]
    fn a_nested_backtick_template_is_one_token() {
        let src = "var p = `{g:choice(A: `{x:int}`, B: word)}`";
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
            "var p = `{a:choice(A: `{b:choice(C: `{c:int}`)}`)}`",
            "var p = `{a:choice(A: `{x:int}`, B: `{y:word}`)}`",
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
        let (_, diags) = lex_text(r"var p = `a\`b`");
        assert!(diags.is_empty(), "{diags:?}");

        // And an outer template that never closes still faults.
        let (_, diags) = lex_text("var p = `{g:choice(A: `{x:int}`)}");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].kind(), DiagCode::UnterminatedTemplate);
    }

    /// A brace inside a **string literal** is text, not structure: a brace
    /// counter with no string arm leaves `one_of("{")` — a legal §7.5 program —
    /// unbalanced at the closing backtick, which then reads as an *opener*.
    /// This is what the rule shared with the input parser's scanner prevents.
    #[test]
    fn a_brace_inside_a_string_does_not_extend_the_template() {
        for template in [
            r#"`{c:one_of("{")}`"#,
            r#"`{c:one_of("}")}`"#,
            r#"`{s:sep("{", int)}`"#,
            r#"`{c:one_of("`")}`"#,
        ] {
            let src = format!("var p = {template}\nvar q = 1\n");
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

    /// A lexer walks whatever the file contains, so nesting is bounded — and the
    /// bound is *exactly* [`praxis_syntax::MAX_TEMPLATE_NESTING`]: a properly
    /// closed nest of `MAX_TEMPLATE_NESTING` templates is **one** token, and one
    /// level deeper is not.
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
        let (kinds, diags) = lex_text(&format!("var p = {at_the_bound}\nvar q = 1\n"));
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
        let (_, diags) = lex_text(&format!("var p = {past}\nvar q = 1\n"));
        assert!(
            diags
                .iter()
                .any(|d| d.kind() == DiagCode::UnterminatedTemplate),
            "one level past the bound the innermost template is not entered — an \
             unbounded lexer reports nothing here"
        );

        // And the pathological case reports rather than overflowing the stack.
        let (_, diags) = lex_text(&format!("var p = {}", "`{a:".repeat(5_000)));
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
        let id = map.intern("day.px", "var @ = 1");
        let out = lex(id, "var @ = 1");
        let rendered = praxis_source::render_one(&map, &out.diagnostics[0]);
        insta::assert_snapshot!(rendered, @r"
error[T003]: unexpected character in source

  day.px:1:5
  1 | var @ = 1
    |     ^ unexpected character in source
");
    }

    // ---- Keywords, builtins and identifier runs ----

    #[test]
    fn keywords_split_from_identifiers() {
        let (kinds, diags) = lex_text(
            "var fn if else while for in loop match return break continue read struct enum true false",
        );
        assert!(diags.is_empty());
        for keyword in [
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

    /// Identifier continuation is a per-*scalar* question (§4.1): a scalar that
    /// is not ident-continue ends the run rather than extending it.
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
        let (kinds, diags) = lex_text("var λ = 1");
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

    // --- the newline fact the parser needs (ADR-049) ------------------------

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
            newline_flags("var a\nvar b"),
            vec![
                (KW_VAR, false),
                (Ident, false),
                (KW_VAR, true),
                (Ident, false),
                (EOF, false),
            ]
        );
    }

    /// Two statements on one line report no line break anywhere — which is
    /// exactly what the parser's statement-separator check has to be able to
    /// see.
    #[test]
    fn same_line_tokens_report_no_newline() {
        assert!(newline_flags("var a = 1 var b = 2")
            .iter()
            .all(|(_, newline)| !newline));
    }

    /// The fact belongs to the whole trivia run, not to its last token: a
    /// comment after the break must not hide it.
    #[test]
    fn a_line_break_anywhere_in_the_trivia_run_counts() {
        let flags = newline_flags("var a\n  // why\n  var b");
        assert_eq!(flags[2], (SyntaxKind::KW_VAR, true));

        let commented = newline_flags("1 /* over\ntwo lines */ + 2");
        assert_eq!(commented[1], (SyntaxKind::PLUS, true));
    }

    /// A line comment ends its line by construction, so it reads as a break even
    /// when EOF eats the `\n` that would otherwise follow.
    #[test]
    fn a_line_comment_ends_the_line_it_is_on() {
        let flags = newline_flags("var a // trailing");
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

    // --- string interpolation (§8.1, ADR-147) -------------------------------

    /// Lex `text` and return `(kind, source text)` for every meaningful token,
    /// which is how the fragment shape is asserted: the fragments carry their
    /// own delimiters, so the token texts are the whole story.
    fn lex_pieces(text: &str) -> Vec<(SyntaxKind, String)> {
        let map = SourceMap::new();
        let id = map.intern("test.px", text);
        lex(id, text)
            .tokens
            .into_iter()
            .filter(|t| !t.kind.is_trivia() && t.kind != SyntaxKind::EOF)
            .map(|t| {
                (
                    t.kind,
                    text[t.span.start().to_u32() as usize..t.span.end().to_u32() as usize]
                        .to_string(),
                )
            })
            .collect()
    }

    /// A literal with no brace in it is one token, one kind, no fragments.
    #[test]
    fn a_literal_with_no_hole_is_still_one_text_lit() {
        assert_eq!(
            lex_pieces(r#""hello""#),
            vec![(SyntaxKind::TextLit, r#""hello""#.to_string())]
        );
        // A `}` in text closes nothing, so this is not a fragment either.
        assert_eq!(
            lex_pieces(r#""a } b""#),
            vec![(SyntaxKind::TextLit, r#""a } b""#.to_string())]
        );
    }

    /// **The gate for ADR-147 decision 1.** A name inside a hole is an ordinary
    /// `Ident` token *at its own range*, not a substring of one opaque literal.
    ///
    /// That is the whole representation decision. `praxis-hir`'s capture
    /// analysis finds free variables by looking token ranges up in the
    /// resolver's map, so an implementation that kept the literal whole and
    /// re-lexed holes later would leave `|_| "{outer}"` capturing nothing — a
    /// silent wrong answer, not a compile error. This asserts the range, not
    /// merely the kind, because the kind alone would pass for a token the lexer
    /// synthesized at the wrong offset.
    #[test]
    fn a_name_in_a_hole_is_a_token_at_its_own_range() {
        let src = r#""Part 2: {part2}""#;
        let map = SourceMap::new();
        let id = map.intern("test.px", src);
        let ident = lex(id, src)
            .tokens
            .into_iter()
            .find(|t| t.kind == SyntaxKind::Ident)
            .expect("the hole's name is an Ident token");
        let start = ident.span.start().to_u32() as usize;
        let end = ident.span.end().to_u32() as usize;
        assert_eq!(&src[start..end], "part2");
        assert_eq!(start, src.find("part2").unwrap());
    }

    /// The three fragment kinds, each carrying one delimiter at each end.
    #[test]
    fn an_interpolated_literal_is_fragments_around_ordinary_tokens() {
        assert_eq!(
            lex_pieces(r#""a{x}b{y}c""#),
            vec![
                (SyntaxKind::InterpOpen, r#""a{"#.to_string()),
                (SyntaxKind::Ident, "x".to_string()),
                (SyntaxKind::InterpMiddle, "}b{".to_string()),
                (SyntaxKind::Ident, "y".to_string()),
                (SyntaxKind::InterpClose, r#"}c""#.to_string()),
            ]
        );
    }

    /// A hole holds a full expression, so its tokens are the tokens that
    /// expression has anywhere else — operators, calls, subscripts and all.
    #[test]
    fn a_hole_holds_ordinary_expression_tokens() {
        let kinds: Vec<SyntaxKind> = lex_pieces(r#""{a + b}""#)
            .into_iter()
            .map(|p| p.0)
            .collect();
        assert_eq!(
            kinds,
            vec![
                SyntaxKind::InterpOpen,
                SyntaxKind::Ident,
                SyntaxKind::PLUS,
                SyntaxKind::Ident,
                SyntaxKind::InterpClose,
            ]
        );
    }

    /// A `{` inside a hole is an ordinary brace and the `}` matching it does not
    /// close the hole — which is what the per-hole depth counter is for. Without
    /// it `"{if c { 1 } else { 2 }}"` would end at the first `}` and the rest of
    /// the line would be lexed as source nobody wrote.
    #[test]
    fn a_brace_inside_a_hole_nests_rather_than_closing_it() {
        let pieces = lex_pieces(r#""{if c { 1 } else { 2 }}""#);
        assert_eq!(pieces.first().unwrap().0, SyntaxKind::InterpOpen);
        assert_eq!(
            pieces.last().unwrap(),
            &(SyntaxKind::InterpClose, r#"}""#.to_string())
        );
        assert_eq!(
            pieces
                .iter()
                .filter(|p| p.0 == SyntaxKind::L_BRACE || p.0 == SyntaxKind::R_BRACE)
                .count(),
            4,
            "the two blocks' braces are ordinary braces"
        );
    }

    /// A `"` inside a hole opens a literal of its own, and a hole inside *that*
    /// pushes a second frame. Only the innermost frame is ever consulted, so
    /// nesting falls out of the stack rather than being arranged.
    #[test]
    fn a_literal_nested_in_a_hole_is_its_own_run() {
        let kinds: Vec<SyntaxKind> = lex_pieces(r#""{m["k"]}""#)
            .into_iter()
            .map(|p| p.0)
            .collect();
        assert_eq!(
            kinds,
            vec![
                SyntaxKind::InterpOpen,
                SyntaxKind::Ident,
                SyntaxKind::L_BRACK,
                SyntaxKind::TextLit,
                SyntaxKind::R_BRACK,
                SyntaxKind::InterpClose,
            ]
        );
        // …and the nested literal may itself interpolate.
        let nested = lex_pieces(r#""{f("{y}")}""#);
        assert_eq!(
            nested
                .iter()
                .filter(|p| p.0 == SyntaxKind::InterpOpen)
                .count(),
            2
        );
    }

    /// **The gate for ADR-147 decision 5.** An unterminated interpolated literal
    /// is the *fallback*: one `TextLit` and one `T004`, exactly what an
    /// unterminated plain literal is. No fragment is emitted, so nothing is left
    /// on the lexer's hole stack and the rest of the file lexes normally.
    ///
    /// An implementation that emitted the opening fragment first and discovered
    /// the problem afterwards passes every other test here and produces a
    /// cascade on this one.
    #[test]
    fn an_unterminated_interpolated_literal_is_one_text_lit_and_t004() {
        for src in ["\"a {b\ncd\n", "\"{a\n", "\"{a // b}\"\n"] {
            let (kinds, diags) = lex_text(src);
            assert_eq!(diags.len(), 1, "{src:?}");
            assert_eq!(
                diags[0].kind(),
                DiagCode::UnterminatedTextLiteral,
                "{src:?}"
            );
            assert!(kinds.contains(&SyntaxKind::TextLit), "{src:?}");
            assert!(
                !kinds.contains(&SyntaxKind::InterpOpen),
                "no fragment is emitted for a literal that never closes: {src:?}"
            );
        }
    }

    /// `\{` is a literal brace, so it opens no hole and earns no `T005`
    /// (ADR-147 decision 4). `\}` is accepted for symmetry.
    #[test]
    fn an_escaped_brace_is_literal_text() {
        let (kinds, diags) = lex_text(r#""\{ and \}""#);
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(kinds.first(), Some(&SyntaxKind::TextLit));
    }

    /// Escapes are still validated in every fragment, not just in a whole
    /// literal — and only in the *literal text*, since a hole holds expression
    /// tokens where a backslash is not an escape at all.
    #[test]
    fn an_escape_is_validated_in_every_fragment() {
        let (_, diags) = lex_text(r#""\q{x}\z""#);
        assert_eq!(diags.len(), 2, "{diags:?}");
        assert!(diags.iter().all(|d| d.kind() == DiagCode::InvalidEscape));
    }

    // --- the character literal (ADR-141) ---

    /// A `'…'` literal is one `CharLit` token spanning both quotes.
    #[test]
    fn a_char_literal_is_one_token() {
        let map = SourceMap::new();
        let src = "var c = '#'";
        let id = map.intern("test.px", src);
        let out = lex(id, src);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let lit = out
            .tokens
            .iter()
            .find(|t| t.kind == SyntaxKind::CharLit)
            .expect("a CharLit token");
        assert_eq!(
            &src[lit.span.start().to_usize()..lit.span.end().to_usize()],
            "'#'"
        );
    }

    /// The escape set is the text literal's plus `\'`, and **no more**: a `\u`
    /// is `T005` here exactly as it is inside `"…"`. Pinning both halves is what
    /// keeps the two tables from drifting into two languages.
    #[test]
    fn the_char_escapes_are_the_text_escapes_plus_the_quote() {
        for src in [
            r"var c = '\n'",
            r"var c = '\r'",
            r"var c = '\t'",
            r"var c = '\0'",
            r"var c = '\\'",
            r"var c = '\''",
            r#"var c = '\"'"#,
        ] {
            let (kinds, diags) = lex_text(src);
            assert!(diags.is_empty(), "{src}: {diags:?}");
            assert!(kinds.contains(&SyntaxKind::CharLit), "{src}");
        }
        for src in [r"var c = '\q'", r"var c = '\u{41}'", r"var c = '\x41'"] {
            let (_, diags) = lex_text(src);
            assert!(
                diags.iter().any(|d| d.kind() == DiagCode::InvalidEscape),
                "{src}: {diags:?}"
            );
        }
    }

    /// `"##"[0]` is a well-typed program that quietly means `#`; `'##'` is a
    /// lexical error carrying the rewrite the author meant.
    #[test]
    fn a_two_character_char_literal_is_a_lex_error() {
        let (kinds, diags) = lex_text("var c = 'ab'");
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert_eq!(diags[0].kind(), DiagCode::CharLiteralIsNotOneCharacter);
        assert_eq!(
            diags[0].message(),
            "a character literal holds exactly one character"
        );
        let fix = diags[0].suggestions().first().expect("a fix-it");
        assert_eq!(fix.replacement.as_deref(), Some("\"ab\""));
        // Lossless even when refused (ADR-003).
        assert!(kinds.contains(&SyntaxKind::CharLit));
    }

    /// …and the empty one, where `""[0]` is an index fault at run time.
    #[test]
    fn an_empty_char_literal_is_a_lex_error() {
        let (kinds, diags) = lex_text("var c = ''");
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert_eq!(diags[0].kind(), DiagCode::CharLiteralIsNotOneCharacter);
        assert_eq!(
            diags[0].message(),
            "empty character literal: `''` names no character"
        );
        assert!(kinds.contains(&SyntaxKind::CharLit));
    }

    #[test]
    fn an_unterminated_char_literal_ends_at_its_line() {
        let (kinds, diags) = lex_text("var c = 'a\nvar d = 1\n");
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert_eq!(diags[0].kind(), DiagCode::UnterminatedCharLiteral);
        assert!(kinds.contains(&SyntaxKind::CharLit));
        // The rest of the file is still lexed: the `var d` line is intact.
        assert_eq!(
            kinds.iter().filter(|k| **k == SyntaxKind::KW_VAR).count(),
            2
        );
    }

    /// A scan that advanced by byte would count `'é'` as two characters and
    /// report a literal that names one.
    #[test]
    fn a_multibyte_scalar_is_one_character() {
        for src in ["var c = 'é'", "var c = '😀'", "var c = '字'"] {
            let (kinds, diags) = lex_text(src);
            assert!(diags.is_empty(), "{src}: {diags:?}");
            assert!(kinds.contains(&SyntaxKind::CharLit), "{src}");
        }
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

    /// A `_` between digits belongs to the literal, in every digit run.
    ///
    /// The token text keeps the separators; removing them is the decoder's half.
    #[test]
    fn a_digit_separator_belongs_to_the_literal() {
        for (src, kind) in [
            ("1_000", SyntaxKind::IntLit),
            ("1_0_0", SyntaxKind::IntLit),
            // A run is one separator: nothing here counts `_`s.
            ("1__0", SyntaxKind::IntLit),
            // A long run of separators, past `Int`'s range.
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
    /// `1_` is the literal `1` followed by the `UNDERSCORE` token — which the
    /// parser rejects where it stands. A separator has digits on *both* sides,
    /// so the number never ends in punctuation.
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

    /// A digit run immediately after a `.` is a **tuple index**, so it takes no
    /// fractional part: `t.0.1` is two indices, not an index and the float
    /// `0.1`.
    ///
    /// The rule is adjacency to a bare `DOT` **token**, which is why `1.5..2.5`
    /// is untouched: the byte before its `2` is a `.` too, but that one was
    /// consumed into a `DOT2`.
    #[test]
    fn a_digit_run_after_a_dot_is_an_index_and_takes_no_fraction() {
        // Two indices in a row.
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
        for src in ["3.0", "1.5", "var x = 0.25", "1.5e3", "3.141_592"] {
            let (kinds, diags) = lex_text(src);
            assert!(diags.is_empty(), "{src}: {diags:?}");
            assert!(kinds.contains(&SyntaxKind::FloatLit), "{src}: {kinds:?}");
        }
        // The case that makes the rule a *token* rule: the byte before `2` is
        // the second `.` of the `..`, but that `.` is inside a `DOT2`.
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

    /// A separated bound is still a bound: `1_000..2_000` is a range, not a
    /// float and not one token.
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
