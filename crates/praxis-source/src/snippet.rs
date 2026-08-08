//! Shared source-snippet rendering: the `path:line:col` header, the numbered
//! source line(s) a span touches, and a caret underline.
//!
//! Both the compiler [`Renderer`](crate::diagnostic::Renderer) and the crash
//! debugger's `render_source_span` build their snippets through this module, so
//! the caret behavior (clamping to the visible line, multi-line span handling)
//! stays identical across the two surfaces and is tested in one place.
//!
//! Design notes:
//!
//! - A span's carets never overrun the visible line. The earlier renderer drew
//!   `max(1, end − start)` carets all on the *first* line, so a multi-line span
//!   (e.g. the whole `fn`) produced a caret line far wider than the source —
//!   the runaway `^^^…` seen in M2. We now clamp the underline on each rendered
//!   line to that line's content extent.
//! - For a span that crosses several lines we underline each covered line:
//!   from the span start to the line end on the first line, the full content
//!   width on middle lines, and from the line start to the span end on the last
//!   line (the rustc/ariadne convention). The number of rendered lines is
//!   capped so a pathological whole-file span stays readable.
//! - The `message` (when non-empty) trails the carets on the *first* underlined
//!   line, matching §8.2 (`^^^^ this value is Text`).
//! - **A column is a count of characters, not of bytes** (REP-35). Spans are
//!   byte ranges — that is what every other layer needs — but a rendered column
//!   is a position in the line as it is *printed*, and the two only coincide in
//!   ASCII. The renderer used the byte offset directly for the header column,
//!   for the caret padding and for the caret run, so one `λ` earlier on the
//!   line pushed the caret one column right, and a `λ` under the caret drew two
//!   carets for one character. Every caret in every diagnostic on a line
//!   holding non-ASCII text was wrong. The conversion belongs here, not in
//!   `LineMap`: `LineCol` round-trips through `linecol_to_offset` and is a byte
//!   column on purpose.
//! - **A header column is 1-based, like its line** (REP-62). `LineCol::col`
//!   counts from zero so it can be inverted; the *header* printed that number
//!   raw, so an error on a file's first character read `f.px:1:0` — a 1-based
//!   line beside a 0-based column, and one off from every other compiler's
//!   `line:col`. The `+ 1` is applied here, at the one place a column is
//!   printed, and nothing about `LineMap` changed. The caret padding keeps the
//!   0-based number: it is a count of characters to skip, not a position.

use std::fmt::Write;

use crate::file::SourceFile;
use crate::line_map::{LineCol, LineMap};
use crate::span::{BytePos, FileSpan};
use crate::style::{Palette, Severity as StyleSeverity, Style};

/// The maximum number of source lines a single snippet may render before it is
/// collapsed. A span covering more lines shows its first `HEAD_LINES` lines and
/// its final line, with an ellipsis between, so a whole-file span stays a
/// glance, not a dump.
pub const MAX_SNIPPET_LINES: usize = 5;
/// Lines kept at the head of a collapsed multi-line span before the ellipsis.
const HEAD_LINES: u32 = 3;

/// A secondary underline label carried alongside a caret run. `Plain` draws
/// just the carets; `Labelled(text)` appends ` {text}` after them on the first
/// underlined line (the §8.2 `^^^^ this value is Text` form).
#[derive(Clone, Copy, Default)]
pub enum CaretLabel<'a> {
    /// No trailing text — carets only.
    #[default]
    Plain,
    /// Append `text` after the carets on the first underlined line.
    Labelled(&'a str),
}

impl<'a> CaretLabel<'a> {
    fn text(self) -> Option<&'a str> {
        match self {
            CaretLabel::Plain => None,
            CaretLabel::Labelled(s) => Some(s),
        }
    }
}

/// Render the `path:line:col` header followed by the numbered source line(s)
/// the span touches, with a clamped caret underline.
///
/// `label` is shown after the carets on the first underlined line. The output
/// is appended to `out` with no leading/trailing blank line; callers frame it.
///
/// The layout matches §8.2:
///
/// ```text
///   day03.px:18:14
///   18 | total += line
///      |          ^^^^ this value is Text
/// ```
pub fn render_span_snippet(
    file: &SourceFile,
    span: FileSpan,
    label: CaretLabel<'_>,
    out: &mut String,
) {
    render_span_snippet_with_limits(file, span, label, out, MAX_SNIPPET_LINES);
}

/// Like [`render_span_snippet`], but renders up to `max_lines` source lines
/// before collapsing the rest to an ellipsis. Pass a large `max_lines` (e.g.
/// `u32::MAX`) to disable collapsing entirely — the crash debugger uses this for
/// its `source` command, where the whole function body (including the faulting
/// line) must be visible, not elided.
pub fn render_span_snippet_with_limits(
    file: &SourceFile,
    span: FileSpan,
    label: CaretLabel<'_>,
    out: &mut String,
    max_lines: usize,
) {
    render_span_snippet_styled(file, span, label, out, max_lines, &Palette::plain(), None);
}

/// The fully-parameterized snippet renderer: `palette` controls ANSI styling,
/// `sev` (when `Some`) colors the caret run in the matching severity color.
/// `sev = None` draws plain carets (used by the crash debugger and notes).
pub fn render_span_snippet_styled(
    file: &SourceFile,
    span: FileSpan,
    label: CaretLabel<'_>,
    out: &mut String,
    max_lines: usize,
    palette: &Palette,
    sev: Option<StyleSeverity>,
) {
    let line_map = file.line_map();
    let text = file.text();
    let start = span.span.start();
    let end = span.span.end();
    let LineCol {
        line: start_line,
        col: byte_col,
    } = line_map.offset_to_linecol(start);
    // The header column is the printed one, so it agrees with the caret below
    // it. `byte_col` is the distance from the line start in bytes, which is how
    // the line start is recovered.
    //
    // **`+ 1` because a header column is 1-based** (REP-62). `LineCol::col`
    // counts from zero — deliberately, so `linecol_to_offset` can invert it —
    // and printing it raw put a 0-based column beside a 1-based line, so an
    // error on a file's first character read `f.px:1:0` and every header
    // disagreed with the caret drawn under it by one. The caret is unchanged:
    // it is drawn from a *count* of characters to skip, which is the 0-based
    // number.
    let col = char_width(text, BytePos(start.to_u32() - byte_col), start) + 1;

    out.push('\n');
    let loc = palette.paint(
        Style::Location,
        &format!("  {}:{}:{}", file.path().display(), start_line, col),
    );
    let _ = writeln!(out, "{loc}");

    // A zero-length span still underlines its single position; a non-empty span
    // may cross several lines. The end's line is the line of its last covered
    // byte (end is exclusive, so end-1 is the last byte inside the span). A span
    // end past EOF (e.g. a synthetic wide span) is clamped to the last real
    // content byte so it does not map to a phantom line after the final newline.
    let text_len = text.len() as u32;
    let last_content = end.to_u32().min(text_len).saturating_sub(1);
    let end_line = if end > start && last_content >= start.to_u32() {
        line_map.offset_to_linecol(BytePos(last_content)).line
    } else {
        start_line
    };

    let span_lines = end_line.saturating_sub(start_line) + 1;
    let ellide = span_lines as usize > max_lines;
    let last_line = end_line;

    let mut first_underline_done = false;
    let mut line = start_line;
    while line <= last_line {
        // When there are too many lines, show the first HEAD_LINES then jump to
        // the final line with an ellipsis separator.
        if ellide && line > start_line + HEAD_LINES - 1 && line < last_line {
            let _ = writeln!(out, "  ...");
            line = last_line;
            continue;
        }

        let (line_text, line_start, content_end) = line_text(text, line_map, line);
        let gutter = palette.paint(Style::Location, &format!("  {line} | "));
        let _ = writeln!(out, "{gutter}{line_text}");

        render_caret_line(
            out,
            text,
            line,
            last_line,
            start,
            end,
            line_start,
            content_end,
            line == start_line,
            &mut first_underline_done,
            label,
            palette,
            sev,
        );

        line += 1;
    }
}

/// Render the caret underline for a single source line, clamped to that line's
/// intersection with `[start, end)`.
#[allow(clippy::too_many_arguments)]
fn render_caret_line(
    out: &mut String,
    text: &str,
    line: u32,
    last_line: u32,
    start: BytePos,
    end: BytePos,
    line_start: BytePos,
    content_end: BytePos,
    is_start_line: bool,
    first_underline_done: &mut bool,
    label: CaretLabel<'_>,
    palette: &Palette,
    sev: Option<StyleSeverity>,
) {
    let multi = last_line != line || (end > content_end && is_start_line);
    // Only draw a caret if this line intersects the span. An empty span draws a
    // single caret on its start line.
    let seg_start = start.max(line_start);
    let seg_end = if end == start {
        // Zero-length span: a single caret at the position.
        seg_start
    } else {
        end.min(content_end).max(seg_start)
    };
    if seg_end < seg_start && !(end == start && is_start_line) {
        return;
    }

    let gutter_width = last_line.to_string().len();
    let pad: String = " ".repeat(gutter_width);
    let gutter = palette.paint(Style::Location, &format!("  {pad} | "));
    let _ = write!(out, "{gutter}");
    // **Characters, not bytes** (REP-35). The padding has to be as wide as the
    // line's text is *printed*, and the caret run as wide as what it underlines.
    let caret_col = char_width(text, line_start, seg_start);
    for _ in 0..caret_col {
        out.push(' ');
    }
    let count = if end == start {
        1
    } else {
        char_width(text, seg_start, seg_end).max(1)
    };
    let carets = "^".repeat(count);
    let carets = match sev {
        Some(s) => palette.paint(Style::Caret(s), &carets),
        None => carets,
    };
    let _ = write!(out, "{carets}");
    if !*first_underline_done {
        if let Some(msg) = label.text() {
            let _ = write!(out, " {msg}");
        }
        *first_underline_done = true;
    }
    if multi {
        let _ = write!(out, "...");
    }
    out.push('\n');
}

/// How many **characters** `text[from..to]` holds — the printed width of a byte
/// range, which is what a column and a caret run are measured in.
///
/// Out-of-range or non-boundary offsets fall back to the byte count: a caret
/// that is one column off is better than a panic, and every caller here passes
/// offsets that came from a span, which are boundaries.
fn char_width(text: &str, from: BytePos, to: BytePos) -> usize {
    let lo = from.to_u32() as usize;
    let hi = (to.to_u32() as usize).max(lo);
    text.get(lo..hi)
        .map_or_else(|| hi - lo, |slice| slice.chars().count())
}

/// The trimmed text of `line` (1-based) and its `[line_start, content_end)`
/// byte extent. `content_end` excludes the line terminator.
fn line_text<'a>(text: &'a str, line_map: &LineMap, line: u32) -> (&'a str, BytePos, BytePos) {
    let (line_start, line_end) = line_map
        .line_range(line)
        .unwrap_or((BytePos::ZERO, BytePos::ZERO));
    let bytes = text.as_bytes();
    let s = line_start.to_usize();
    let e = (line_end.to_u32() as usize).min(bytes.len());
    let line_text = std::str::from_utf8(&bytes[s..e])
        .unwrap_or("<invalid utf-8>")
        .trim_end_matches(['\n', '\r']);
    let content_end = LineMap::trim_line_terminator(bytes, line_start, line_end);
    (line_text, line_start, content_end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::SourceMap;
    use crate::span::Span;

    fn render(file_text: &str, start: u32, end: u32, label: CaretLabel<'_>) -> String {
        let map = SourceMap::new();
        let id = map.intern("f.px", file_text);
        let file = map.get(id).unwrap();
        let span = FileSpan::new(id, Span::new(start, end));
        let mut out = String::new();
        render_span_snippet(&file, span, label, &mut out);
        out
    }

    #[test]
    fn single_line_span_with_label() {
        // "total += line" — "line" at 9..13.
        let out = render(
            "total += line\n",
            9,
            13,
            CaretLabel::Labelled("this value is Text"),
        );
        // The caret `|` lines up under the source line's `|` (both at column 4).
        insta::assert_snapshot!(out, @r#"
  f.px:1:10
  1 | total += line
    |          ^^^^ this value is Text
"#);
    }

    #[test]
    fn single_line_span_plain() {
        let out = render("ab\ncd\n", 0, 1, CaretLabel::Plain);
        insta::assert_snapshot!(out, @r#"
  f.px:1:1
  1 | ab
    | ^
"#);
    }

    #[test]
    fn multi_line_span_underlines_each_line() {
        let src = "fn main() -> Int {\n    out(\"x\")\n}\n";
        let end = src.len() as u32;
        let out = render(src, 0, end, CaretLabel::Plain);
        insta::assert_snapshot!(out, @r#"
  f.px:1:1
  1 | fn main() -> Int {
    | ^^^^^^^^^^^^^^^^^^...
  2 |     out("x")
    | ^^^^^^^^^^^^...
  3 | }
    | ^
"#);
    }

    #[test]
    fn huge_span_collapses_to_head_plus_last() {
        let src = "a\nb\nc\nd\ne\nf\ng\nh\n";
        let end = src.len() as u32;
        let out = render(src, 0, end, CaretLabel::Plain);
        insta::assert_snapshot!(out, @r#"
  f.px:1:1
  1 | a
    | ^...
  2 | b
    | ^...
  3 | c
    | ^...
  ...
  8 | h
    | ^
"#);
    }

    #[test]
    fn caret_never_overflows_visible_line() {
        // Span deliberately extends past line 1's content into line 2. The
        // caret on line 1 must stop at the line end, not run on past "short",
        // and a past-EOF end must not invent a phantom line 3.
        let src = "short\nnext line\n";
        let out = render(src, 2, 99, CaretLabel::Plain);
        insta::assert_snapshot!(out, @r#"
  f.px:1:3
  1 | short
    |   ^^^...
  2 | next line
    | ^^^^^^^^^
"#);
    }

    /// **REP-35.** A column is a count of characters, not of bytes.
    ///
    /// The renderer used the span's byte offset as the display column, so every
    /// multi-byte character earlier on the line pushed the caret one column
    /// right per *extra* UTF-8 byte — and a multi-byte character under the
    /// caret drew one caret per byte. Every caret in every diagnostic on a line
    /// holding non-ASCII text was wrong, on this branch and on `main`.
    ///
    /// `λ` is two bytes, so `name` sits at **byte** 15 and **column** 13 — and
    /// the caret used to be drawn at column 15, two past the `n`.
    #[test]
    fn a_caret_counts_characters_and_not_bytes() {
        let src = "var y = λλ + name\n";
        let start = src.find("name").expect("the needle") as u32;
        assert_eq!(start, 15, "the byte offset is what a span carries");
        let out = render(src, start, start + 4, CaretLabel::Plain);
        insta::assert_snapshot!(out, @r#"
  f.px:1:14
  1 | var y = λλ + name
    |              ^^^^
"#);

        // And the run itself: two `λ` under the caret are two carets, not four.
        let out = render(src, 8, 12, CaretLabel::Plain);
        insta::assert_snapshot!(out, @r#"
  f.px:1:9
  1 | var y = λλ + name
    |         ^^
"#);
    }

    #[test]
    fn empty_span_draws_single_caret() {
        let out = render("abc\n", 1, 1, CaretLabel::Plain);
        insta::assert_snapshot!(out, @r#"
  f.px:1:2
  1 | abc
    |  ^
"#);
    }

    /// **REP-62.** A header column is 1-based, like its line, and it names the
    /// character the caret is drawn under.
    ///
    /// `LineCol::col` counts from zero so `linecol_to_offset` can invert it, and
    /// the header printed that number raw — so a span on a file's very first
    /// character read `f.px:1:0`: a 1-based line beside a 0-based column, one
    /// off from the caret below it and from every other compiler's `line:col`.
    ///
    /// Asserted as a *relation* rather than as another literal snapshot: the
    /// column is read back out of the header and used to index the source line,
    /// so the property survives a reworded header, and the caret's own offset is
    /// checked against it. The snapshots above pin the rendering; this pins what
    /// the number means.
    #[test]
    fn a_header_column_is_one_based_and_lands_on_the_caret() {
        for (src, start, len, expect_char) in [
            ("abc\n", 0u32, 1u32, 'a'),
            ("abc\n", 2, 1, 'c'),
            ("total += line\n", 9, 4, 'l'),
            // A multi-byte character earlier on the line: the column counts
            // characters (REP-35), so the `+ 1` must not be applied to bytes.
            ("var y = λλ + name\n", 15, 4, 'n'),
        ] {
            let out = render(src, start, start + len, CaretLabel::Plain);
            let header = out.lines().find(|l| l.contains("f.px:")).expect("header");
            let column: usize = header
                .rsplit(':')
                .next()
                .expect("a column")
                .trim()
                .parse()
                .expect("the column is a number");
            assert!(
                column >= 1,
                "a column is 1-based, got {column} in {header:?}"
            );
            let line: Vec<char> = src.lines().next().expect("a line").chars().collect();
            assert_eq!(
                line[column - 1],
                expect_char,
                "column {column} of {src:?} should be {expect_char:?}"
            );
            // And the caret line skips exactly `column - 1` characters after the
            // `    | ` gutter, so header and caret name the same character.
            let caret_line = out.lines().find(|l| l.contains('^')).expect("a caret");
            let carets_at = caret_line.find('^').expect("a caret") - "    | ".len();
            assert_eq!(
                carets_at,
                column - 1,
                "the caret in {caret_line:?} disagrees with the header {header:?}"
            );
        }
    }
}
