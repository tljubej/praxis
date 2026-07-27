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
        col,
    } = line_map.offset_to_linecol(start);

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
    let caret_col = seg_start.to_u32().saturating_sub(line_start.to_u32());
    for _ in 0..caret_col {
        out.push(' ');
    }
    let count = if end == start {
        1
    } else {
        (seg_end.to_u32().saturating_sub(seg_start.to_u32()) as usize).max(1)
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
    let content_end = trim_one_terminator(bytes, line_start, line_end);
    (line_text, line_start, content_end)
}

/// The content end byte offset of a line: `line_end` minus any single line
/// terminator. Mirrors `line_map`'s internal `trim_one_terminator`.
fn trim_one_terminator(text: &[u8], line_start: BytePos, line_end: BytePos) -> BytePos {
    if line_end <= line_start {
        return line_start;
    }
    let s = line_start.to_usize();
    let e = (line_end.to_u32() as usize).min(text.len());
    let gap = &text[s..e];
    if gap.ends_with(b"\r\n") {
        BytePos(line_end.to_u32() - 2)
    } else if gap.ends_with(b"\n") || gap.ends_with(b"\r") {
        BytePos(line_end.to_u32() - 1)
    } else {
        line_end
    }
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
  f.px:1:9
  1 | total += line
    |          ^^^^ this value is Text
"#);
    }

    #[test]
    fn single_line_span_plain() {
        let out = render("ab\ncd\n", 0, 1, CaretLabel::Plain);
        insta::assert_snapshot!(out, @r#"
  f.px:1:0
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
  f.px:1:0
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
  f.px:1:0
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
  f.px:1:2
  1 | short
    |   ^^^...
  2 | next line
    | ^^^^^^^^^
"#);
    }

    #[test]
    fn empty_span_draws_single_caret() {
        let out = render("abc\n", 1, 1, CaretLabel::Plain);
        insta::assert_snapshot!(out, @r#"
  f.px:1:1
  1 | abc
    |  ^
"#);
    }
}
