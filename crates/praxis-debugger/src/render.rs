//! Crash-snapshot rendering: format a fault + its snapshot into the §9.6
//! noninteractive diagnostic (and the shared text the interactive REPL builds on).
//!
//! The §9.6 noninteractive behavior on a fault is:
//! 1. Print the diagnostic (fault kind; for `ParseFailed`, the §7.11 detail).
//! 2. Print the stack trace (the snapshot's frame chain).
//! 3. Print the top-frame locals up to a configured limit.
//! 4. (The host then exits nonzero.)
//!
//! The interactive REPL (WS5) reuses [`render_backtrace`] and
//! [`render_locals`] for its `bt` and `locals` commands, so the formatting lives
//! here once.

use std::io::Write;

use praxis_runtime::{CrashSnapshot, DebugLocal, FaultKind, ParseDetail};

/// The maximum number of locals the noninteractive fallback prints per frame
/// (§9.6 "up to configured limits"). Keeps the output a useful glance, not a
/// dump; a frame with more locals shows an ellipsis.
const NONINTERACTIVE_LOCAL_LIMIT: usize = 12;

/// Context the locals renderer needs to enrich its output beyond bare names.
///
/// `db` pairs with each local's `type_id` to render the exact static type
/// (`Vec[Int]`, `Map[Text, Int]`, …); without it types degrade to the runtime
/// descriptor's coarse top-level name (or are omitted). `source_text` resolves
/// each temp's span to the `@ "expr"` provenance snippet. Both are optional so
/// the renderer degrades gracefully when invoked without a live session (e.g.
/// the bare noninteractive fallback in some host paths).
pub struct RenderCtx<'a> {
    /// The live `TypeDb` from the program's analysis (positional ids must pair
    /// with the same db the codegen used; never a fresh one).
    pub db: Option<&'a praxis_types::TypeDb>,
    /// The program source text, for resolving temp spans to `@ "expr"`.
    pub source_text: Option<&'a str>,
}

impl<'a> RenderCtx<'a> {
    /// A context with no enrichment (bare names + values, no types/provenance).
    pub fn bare() -> Self {
        RenderCtx {
            db: None,
            source_text: None,
        }
    }

    /// A context wired to the session's `TypeDb` and source text.
    pub fn new(db: &'a praxis_types::TypeDb, source_text: &'a str) -> Self {
        RenderCtx {
            db: Some(db),
            source_text: Some(source_text),
        }
    }
}

/// The width of the frame-number column in the backtrace (e.g. `#0  main`).
const FRAME_NUM_WIDTH: usize = 3;

/// Render the full §9.6 noninteractive diagnostic for a fault into `out`.
///
/// Prints the fault line, the backtrace, and the top frame's locals. A missing
/// snapshot (e.g. a host-side fault before any debug frame was pushed) degrades
/// to just the fault line. The `parse_detail` is consulted only for
/// `FaultKind::ParseFailed` to append the §7.11 input/expected/actual detail.
pub fn render_noninteractive<W: Write>(
    out: &mut W,
    kind: FaultKind,
    snapshot: Option<&CrashSnapshot>,
    parse_detail: Option<&ParseDetail>,
    palette: praxis_source::style::Palette,
    ctx: &RenderCtx<'_>,
) -> std::io::Result<()> {
    use praxis_source::style::{Severity as StyleSeverity, Style};
    // 1. The fault line — a runtime error, colored like a compiler error.
    let label = palette.paint(Style::Severity(StyleSeverity::Error), "error:");
    writeln!(out, "{label} program faulted: {kind}")?;

    // ParseFailed appends the §7.11 detail (input span, expected, actual preview).
    if kind == FaultKind::ParseFailed {
        if let Some(detail) = parse_detail {
            if let Some(fail) = &detail.fail {
                writeln!(
                    out,
                    "       at input offset {}..{}: expected {}",
                    fail.input_span.0, fail.input_span.1, fail.expected
                )?;
                if !detail.actual_preview.is_empty() {
                    writeln!(out, "       actual: {}", detail.actual_preview)?;
                }
            }
        }
    }

    let Some(snap) = snapshot else {
        // No frames to show (host-side fault path). The fault line suffices.
        return Ok(());
    };
    if snap.is_empty() {
        return Ok(());
    }

    // 2. The backtrace.
    writeln!(out)?;
    writeln!(out, "Backtrace:")?;
    render_backtrace(out, snap)?;

    // 3. The top-frame locals (frame 0 = innermost = the faulting function).
    writeln!(out)?;
    render_frame_locals(out, snap, 0, NONINTERACTIVE_LOCAL_LIMIT, ctx)?;
    Ok(())
}

/// Render the snapshot's frame chain as a numbered backtrace (`#0 fn`, `#1 fn`,
/// …), innermost-first. Used by the noninteractive fallback and the REPL `bt`.
pub fn render_backtrace<W: Write>(out: &mut W, snap: &CrashSnapshot) -> std::io::Result<()> {
    for (i, _frame) in snap.frames.iter().enumerate() {
        // SAFETY: function names are compiler-embedded 'static UTF-8.
        let name = unsafe { snap.frame_name(i) };
        writeln!(out, "#{:<width$} {name}", i, width = FRAME_NUM_WIDTH)?;
    }
    Ok(())
}

/// Render the locals of frame `index` (0 = innermost), up to `limit` total,
/// split into two labeled sections: user-written bindings first, then compiler
/// temporaries. Each line shows the local's name (or `<tmp#N: Type>` for a
/// temp), its type when the [`RenderCtx`] carries a `TypeDb`, and the temp's
/// materializing expression as `@ "expr"` when the ctx carries source text.
/// Values are formatted through their descriptors; not-yet-written slots show
/// `<uninit>`. Used by the noninteractive fallback and the REPL `locals`.
///
/// `limit` caps the *total* locals shown across both sections (so a frame with
/// many temps stays a glance, not a dump); an ellipsis notes the remainder.
///
/// # Safety
/// The frame's locals and their descriptors must be valid (the snapshot was
/// deep-copied from a live chain; descriptors are compiler-embedded 'static).
pub fn render_frame_locals<W: Write>(
    out: &mut W,
    snap: &CrashSnapshot,
    index: usize,
    limit: usize,
    ctx: &RenderCtx<'_>,
) -> std::io::Result<()> {
    let Some(frame) = snap.frames.get(index) else {
        writeln!(out, "  <no frame {index}>")?;
        return Ok(());
    };
    if frame.locals.is_empty() {
        writeln!(out, "  (no locals in this frame)")?;
        return Ok(());
    }

    // Partition into user bindings (first) and compiler temps (second),
    // preserving declaration order within each. This replaces the old flat list
    // where temps (`<tmp>`) buried the variables the programmer wrote.
    //
    // Dead scratch temps — ones with neither a current value nor any source
    // provenance — are dropped: they hold nothing and explain nothing (e.g. a
    // closure's hidden `self` arg after the capture prologue, or the function's
    // pre-allocated return slot when the body faulted before returning). A slot
    // that is uninit *but* carries a span is kept: that's a temp for an
    // expression whose value genuinely never computed (the faulting expression
    // itself, e.g. `x / 0`), which is exactly what the user needs to see.
    let mut users: Vec<&DebugLocal> = Vec::new();
    let mut temps: Vec<&DebugLocal> = Vec::new();
    for local in &frame.locals {
        if local.is_user() {
            users.push(local);
        } else if is_real_ref(local.value) || local.span().is_some() {
            temps.push(local);
        }
    }

    // `limit` caps the total shown; give users priority (they are what the
    // programmer is debugging for), then fill remaining slots with temps.
    let total = users.len() + temps.len();
    let user_shown = users.len().min(limit);
    let temp_shown = temps.len().min(limit.saturating_sub(user_shown));
    let shown = user_shown + temp_shown;

    if !users.is_empty() && user_shown > 0 {
        writeln!(out, "  locals:")?;
        for local in &users[..user_shown] {
            render_local_line(out, local, ctx)?;
        }
    }
    if !temps.is_empty() && temp_shown > 0 {
        writeln!(out, "  temps:")?;
        for local in &temps[..temp_shown] {
            render_local_line(out, local, ctx)?;
        }
    }
    if total > shown {
        writeln!(out, "  …({} more)", total - shown)?;
    }
    Ok(())
}

/// Render one local as a single indented line: the label (name or temp tag) +
/// optional `@ "expr"` provenance + `= value`. The label shape depends on the
/// local's kind:
/// - User: `name: Type` (the written name; type omitted if the ctx has no db).
/// - Temp: `<tmp#N: Type>` with the per-frame symbol id and, when source text is
///   available, `@ "expr"` for the expression the temp materialized.
fn render_local_line<W: Write>(
    out: &mut W,
    local: &DebugLocal,
    ctx: &RenderCtx<'_>,
) -> std::io::Result<()> {
    let ty_str = type_str(local, ctx);
    let value_display = if is_real_ref(local.value) {
        format_value(local.value)
    } else {
        "<uninit>".to_string()
    };
    if local.is_user() {
        let name = local.name();
        if name.is_empty() {
            // Defensive: a user-classified local should always have a name.
            writeln!(out, "    ? = {value_display}")?;
        } else if ty_str.is_empty() {
            writeln!(out, "    {name} = {value_display}")?;
        } else {
            writeln!(out, "    {name}: {ty_str} = {value_display}")?;
        }
    } else {
        // Compiler temp: name it by its per-frame id + type, and (when the span
        // resolves against source text) annotate with the materializing expr.
        let tag = if ty_str.is_empty() {
            format!("<tmp#{}>", local.symbol_id)
        } else {
            format!("<tmp#{}: {}>", local.symbol_id, ty_str)
        };
        match provenance(local, ctx) {
            Some(expr) => writeln!(out, "    {tag} @ \"{expr}\" = {value_display}")?,
            None => writeln!(out, "    {tag} = {value_display}")?,
        }
    }
    Ok(())
}

/// The local's type as a string, when the ctx carries the live `TypeDb` and the
/// local's `type_id` resolves within it. Returns an empty string (caller omits
/// the type) when the db is absent, the local has no threaded type (a null
/// descriptor marks a frame constructed before type threading), or the id is
/// out of range. The `type_id` is positional in the program's own `TypeDb`, so
/// it must pair with that same db — never a fresh one (evaluate.rs:184). We do
/// *not* treat `type_id == 0` as "unknown": slot 0 is a legitimate type, and
/// the codegen always threads a valid id for real frames.
fn type_str(local: &DebugLocal, ctx: &RenderCtx<'_>) -> String {
    let Some(db) = ctx.db else {
        return String::new();
    };
    // A null descriptor marks a frame from before type threading (the M5 unit
    // tests) — its `type_id` is not meaningful, so omit the type.
    if local.descriptor.is_null() {
        return String::new();
    }
    let id = local.type_id;
    if id as usize >= db.len() {
        return String::new();
    }
    db.render(praxis_types::Type(id))
}

/// The temp's materializing source expression, when the ctx carries source text
/// and the local's span resolves to a valid slice of it. Returns `None`
/// otherwise (the line degrades to no `@ "..."` annotation). Single-line,
/// whitespace-collapsed for a tidy one-line provenance.
fn provenance(local: &DebugLocal, ctx: &RenderCtx<'_>) -> Option<String> {
    let source = ctx.source_text?;
    let (start, end) = local.span()?;
    let s = usize::try_from(start).ok()?;
    let e = usize::try_from(end).ok()?;
    if s >= source.len() || e > source.len() || s > e {
        return None;
    }
    let raw = &source[s..e];
    let collapsed: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        None
    } else {
        Some(collapsed)
    }
}

/// Format a `GcRef` value through its descriptor (§11.4). Falls back to
/// `<unreadable>` if the descriptor is null or the format produces nothing.
fn format_value(value: praxis_runtime::GcRef) -> String {
    let mut out = String::new();
    // The GcRef's `format` reads its descriptor and writes through it. This is
    // the same path `praxis run` uses to print the program result.
    value.format(&mut out);
    if out.is_empty() {
        "<unreadable>".to_string()
    } else {
        out
    }
}

/// Render the selected frame's source extent (§9.4 `source`, M10b-WS3).
///
/// Prints the lines of `source_text` the frame's `source_span` covers, each
/// prefixed with its 1-based line number, then a clamped caret underline
/// pointing at the span. `(0, 0)` spans (synthetic/span-less frames) print a
/// "no span recorded" note instead. The frame's function name is shown as a
/// header. A span outside `source_text` degrades to the note.
///
/// The caret logic is shared with the compiler via
/// [`praxis_source::snippet::render_span_snippet`], so a multi-line span is
/// underlined on each line and never overruns the visible line (the earlier
/// implementation drew `end − start` carets all on one line).
pub fn render_source_span<W: Write>(
    out: &mut W,
    source_text: &str,
    frame_name: &str,
    span: (u32, u32),
) -> std::io::Result<()> {
    writeln!(out, "{frame_name}:")?;
    let (start, end) = (span.0 as usize, span.1 as usize);
    if start == 0 && end == 0 {
        writeln!(out, "  (no source span recorded for this frame)")?;
        return Ok(());
    }
    if start >= source_text.len() || end > source_text.len() || start > end {
        writeln!(
            out,
            "  (source span {start}..{end} is outside the program source)"
        )?;
        return Ok(());
    }
    // Build a transient source map so the shared snippet helper (clamped carets,
    // multi-line handling) can render this span identically to compiler output.
    let map = praxis_source::SourceMap::new();
    let id = map.intern("<debug>", source_text);
    let Some(file) = map.get(id) else {
        writeln!(out, "  (could not resolve source for this frame)")?;
        return Ok(());
    };
    let file_span = praxis_source::FileSpan::new(
        id,
        praxis_source::Span::new(
            praxis_source::BytePos::from(start as u32),
            praxis_source::BytePos::from(end as u32),
        ),
    );
    let mut buf = String::new();
    // No line limit: the `source` command exists to show the whole function the
    // faulting frame spans, including the faulting line itself — collapsing it
    // to an ellipsis would hide exactly the line the user needs to see.
    praxis_source::snippet::render_span_snippet_with_limits(
        &file,
        file_span,
        praxis_source::snippet::CaretLabel::Plain,
        &mut buf,
        u32::MAX as usize,
    );
    // The shared helper emits a leading newline + indented lines; trim the
    // leading newline so the frame-name header sits directly above the snippet.
    out.write_all(buf.trim_start_matches('\n').as_bytes())?;
    Ok(())
}

/// Render the input near the active parser cursor (§9.4 `input`, M10b-WS3).
///
/// For a `ParseFailed` fault, prints the input bytes around the recorded
/// mismatch offset with a caret under the span. For other fault kinds, prints
/// a "no input context (not a parse failure)" note. The detail's
/// `actual_preview` (already a bounded UTF-8-lossy slice set by the runtime)
/// is shown directly; the input span anchors the caret.
pub fn render_input_context<W: Write>(
    out: &mut W,
    detail: Option<&ParseDetail>,
    input_text: &str,
) -> std::io::Result<()> {
    let Some(detail) = detail else {
        writeln!(out, "(no input context available — session not attached)")?;
        return Ok(());
    };
    let Some(fail) = &detail.fail else {
        writeln!(out, "(no input context — not a parse failure)")?;
        return Ok(());
    };
    let (start, end) = fail.input_span;
    writeln!(out, "input at offset {start}..{end}:")?;
    // The runtime already built a bounded preview; show it (single line).
    if !detail.actual_preview.is_empty() {
        writeln!(out, "  {}", detail.actual_preview)?;
    } else if !input_text.is_empty() {
        // Fall back to the session's input buffer slice if the preview is empty.
        let lo = start.min(input_text.len());
        let hi = end.min(input_text.len()).max(lo);
        writeln!(out, "  {}", &input_text[lo..hi])?;
    } else {
        writeln!(out, "  (empty input)")?;
    }
    Ok(())
}

/// Render the active input-parser context (§9.4 `parser`, M10b-WS3).
///
/// For a `ParseFailed` fault, prints what the parser expected, the parser
/// expression's source span (if threaded), and the actual preview. For other
/// fault kinds, prints a "no parser context" note.
pub fn render_parser_context<W: Write>(
    out: &mut W,
    detail: Option<&ParseDetail>,
    source_text: &str,
) -> std::io::Result<()> {
    let Some(detail) = detail else {
        writeln!(out, "(no parser context available — session not attached)")?;
        return Ok(());
    };
    let Some(fail) = &detail.fail else {
        writeln!(out, "(no parser context — not a parse failure)")?;
        return Ok(());
    };
    writeln!(out, "expected: {}", fail.expected)?;
    if let Some((pstart, pend)) = fail.parser_span {
        let (ps, pe) = (pstart as usize, pend as usize);
        if ps < source_text.len() && pe <= source_text.len() && ps <= pe {
            writeln!(out, "parser expression (source {pstart}..{pend}):")?;
            writeln!(out, "  {}", &source_text[ps..pe])?;
        } else {
            writeln!(
                out,
                "parser expression span: {pstart}..{pend} (outside source)"
            )?;
        }
    } else {
        writeln!(out, "parser expression: <unknown parser>")?;
    }
    Ok(())
}

/// True iff `r` is a real GC reference (not the null sentinel used for
/// not-yet-written debug-local slots). Mirrors the check in crash_snapshot.
fn is_real_ref(r: praxis_runtime::GcRef) -> bool {
    use std::ptr::NonNull;
    let dangling = NonNull::<praxis_runtime::GcHeader>::dangling();
    !std::ptr::eq(r.as_ptr(), dangling.as_ptr())
}

#[cfg(test)]
mod tests {
    use super::*;
    use praxis_runtime::crash_snapshot::SnapshotFrame;

    /// Build a minimal snapshot with one frame named `fn_name` and no locals.
    fn snap_with_frame(fn_name: &str) -> CrashSnapshot {
        let name: &'static str = Box::leak(fn_name.to_string().into_boxed_str());
        let frame = SnapshotFrame {
            parent: usize::MAX,
            func_name: name.as_ptr(),
            func_name_len: name.len() as u32,
            locals: Vec::new(),
            source_span: (0, 0),
        };
        let mut s = CrashSnapshot::new();
        s.fault_kind = FaultKind::IndexOutOfBounds;
        s.frames = vec![frame];
        s
    }

    /// Build a snapshot with one frame carrying the given locals.
    fn snap_with_locals(fn_name: &str, locals: Vec<DebugLocal>) -> CrashSnapshot {
        let name: &'static str = Box::leak(fn_name.to_string().into_boxed_str());
        let frame = SnapshotFrame {
            parent: usize::MAX,
            func_name: name.as_ptr(),
            func_name_len: name.len() as u32,
            locals,
            source_span: (0, 0),
        };
        let mut s = CrashSnapshot::new();
        s.fault_kind = FaultKind::IndexOutOfBounds;
        s.frames = vec![frame];
        s
    }

    /// A `DebugLocal` slot, modeling an uninitialized value (the null sentinel
    /// the runtime uses for not-yet-written slots). `kind` selects user vs temp;
    /// `span` is the optional provenance span; `type_id` pairs with a TypeDb.
    /// `descriptor` is non-null so the type path is exercised (a null descriptor
    /// marks a pre-type-threading frame and suppresses type rendering).
    fn local(
        name: &'static str,
        symbol_id: u32,
        kind: u8,
        span: Option<(u32, u32)>,
        type_id: u32,
    ) -> DebugLocal {
        use std::ptr::NonNull;
        let (span_start, span_end) = span.unwrap_or((0, 0));
        // The null sentinel: a dangling NonNull, never dereferenced. Mirrors
        // `GcRef::null_sentinel_ref` in the runtime (kept crate-private there).
        // SAFETY: a dangling NonNull is non-null and aligned; never deref'd.
        let value = unsafe {
            praxis_runtime::GcRef::from_non_null(NonNull::<praxis_runtime::GcHeader>::dangling())
        };
        // A non-null descriptor placeholder (never dereferenced by the renderer;
        // it only checks for null to decide whether a type_id is meaningful).
        let descriptor: *const praxis_runtime::TypeDescriptor = NonNull::dangling().as_ptr();
        DebugLocal {
            source_name: name.as_ptr(),
            name_len: name.len() as u32,
            symbol_id,
            descriptor,
            value,
            type_id,
            kind,
            span_start,
            span_end,
        }
    }

    #[test]
    fn backtrace_lists_frames_numbered() {
        let snap = snap_with_frame("boom");
        let mut out = Vec::new();
        render_backtrace(&mut out, &snap).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("#0"), "backtrace numbers frames: {text}");
        assert!(text.contains("boom"), "backtrace shows the name: {text}");
    }

    #[test]
    fn noninteractive_renders_fault_and_backtrace() {
        let snap = snap_with_frame("boom");
        let mut out = Vec::new();
        render_noninteractive(
            &mut out,
            snap.fault_kind,
            Some(&snap),
            None,
            praxis_source::style::Palette::plain(),
            &RenderCtx::bare(),
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("program faulted"), "fault line: {text}");
        assert!(text.contains("Backtrace"), "backtrace header: {text}");
        assert!(text.contains("boom"), "frame name: {text}");
    }

    #[test]
    fn noninteractive_without_snapshot_still_shows_fault() {
        let mut out = Vec::new();
        render_noninteractive(
            &mut out,
            FaultKind::DivByZero,
            None,
            None,
            praxis_source::style::Palette::plain(),
            &RenderCtx::bare(),
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("program faulted: division by zero"));
        // No snapshot → no Backtrace section.
        assert!(!text.contains("Backtrace"));
    }

    #[test]
    fn noninteractive_appends_parse_detail_for_parsefailed() {
        let mut detail = ParseDetail::new();
        use praxis_runtime::ParseFail;
        detail.consider(ParseFail::at(5, 0, "int"), b"abc");
        let mut out = Vec::new();
        render_noninteractive(
            &mut out,
            FaultKind::ParseFailed,
            None,
            Some(&detail),
            praxis_source::style::Palette::plain(),
            &RenderCtx::bare(),
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("expected int"), "expected shown: {text}");
        assert!(text.contains("at input offset 5"), "offset shown: {text}");
    }

    #[test]
    fn empty_frame_locals_shows_no_locals_message() {
        let snap = snap_with_frame("main");
        let mut out = Vec::new();
        render_frame_locals(&mut out, &snap, 0, 12, &RenderCtx::bare()).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("no locals"), "{text}");
    }

    // ---- locals split: user bindings vs. compiler temps ----

    #[test]
    fn locals_split_into_users_and_temps_sections() {
        // A frame with one user local `xs` and two temps (#1, #2). The temps
        // carry spans so they are visible (a temp with no value and no span is
        // dead scratch and is filtered out — see dead_scratch_temps_are_hidden).
        let snap = snap_with_locals(
            "main",
            vec![
                local("xs", 0, praxis_runtime::LOCAL_KIND_USER, None, 0),
                local("", 1, praxis_runtime::LOCAL_KIND_TEMP, Some((1, 2)), 0),
                local("", 2, praxis_runtime::LOCAL_KIND_TEMP, Some((3, 4)), 0),
            ],
        );
        let mut out = Vec::new();
        render_frame_locals(&mut out, &snap, 0, 12, &RenderCtx::bare()).unwrap();
        let text = String::from_utf8(out).unwrap();
        // Two labeled sections appear, user first.
        let users_idx = text.find("locals:").expect("locals section");
        let temps_idx = text.find("temps:").expect("temps section");
        assert!(
            users_idx < temps_idx,
            "users section precedes temps: {text}"
        );
        // `xs` is in the locals section; temps are tagged by id.
        assert!(text.contains("xs ="), "user local named: {text}");
        assert!(text.contains("<tmp#1>"), "temp tagged with id: {text}");
        assert!(text.contains("<tmp#2>"), "second temp tagged: {text}");
    }

    #[test]
    fn temp_renders_type_when_db_present() {
        // A TypeDb with Int at slot 1; a temp whose type_id points at it. The
        // temp carries a span so it is visible (dead temps with no value/span
        // are filtered).
        let mut db = praxis_types::TypeDb::new();
        let int_ty = db.int();
        let snap = snap_with_locals(
            "main",
            vec![local(
                "",
                0,
                praxis_runtime::LOCAL_KIND_TEMP,
                Some((0, 1)),
                int_ty.0,
            )],
        );
        let ctx = RenderCtx {
            db: Some(&db),
            source_text: None,
        };
        let mut out = Vec::new();
        render_frame_locals(&mut out, &snap, 0, 12, &ctx).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("<tmp#0: Int>"),
            "temp shows its type from the db: {text}"
        );
    }

    #[test]
    fn temp_omits_type_when_db_absent_or_id_out_of_range() {
        // No db: type omitted, bare `<tmp#N>`. (Span present so the temp shows.)
        let snap = snap_with_locals(
            "main",
            vec![local(
                "",
                0,
                praxis_runtime::LOCAL_KIND_TEMP,
                Some((0, 1)),
                5,
            )],
        );
        let mut out = Vec::new();
        render_frame_locals(&mut out, &snap, 0, 12, &RenderCtx::bare()).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("<tmp#0> ="),
            "no db → bare temp tag without type: {text}"
        );
        // A db is present but the type_id is out of range → type omitted.
        let db = praxis_types::TypeDb::new();
        let snap_oor = snap_with_locals(
            "main",
            vec![local(
                "",
                0,
                praxis_runtime::LOCAL_KIND_TEMP,
                Some((0, 1)),
                999,
            )],
        );
        let ctx = RenderCtx {
            db: Some(&db),
            source_text: None,
        };
        let mut out = Vec::new();
        render_frame_locals(&mut out, &snap_oor, 0, 12, &ctx).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("<tmp#0> =") && !text.contains("<tmp#0:"),
            "out-of-range type_id → no type rendered: {text}"
        );
    }

    #[test]
    fn temp_shows_provenance_when_source_text_present() {
        // Source "xs.get(99)" lives at bytes 0..10; a temp's span points there.
        let src = "xs.get(99)";
        let snap = snap_with_locals(
            "main",
            vec![local(
                "",
                7,
                praxis_runtime::LOCAL_KIND_TEMP,
                Some((0, 10)),
                0,
            )],
        );
        let ctx = RenderCtx {
            db: None,
            source_text: Some(src),
        };
        let mut out = Vec::new();
        render_frame_locals(&mut out, &snap, 0, 12, &ctx).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("@ \"xs.get(99)\""),
            "temp shows its materializing expression: {text}"
        );
    }

    #[test]
    fn temp_omits_provenance_when_span_absent_or_out_of_range() {
        // No span → no `@ "..."`.
        let snap = snap_with_locals(
            "main",
            vec![local("", 0, praxis_runtime::LOCAL_KIND_TEMP, None, 0)],
        );
        let ctx = RenderCtx {
            db: None,
            source_text: Some("anything"),
        };
        let mut out = Vec::new();
        render_frame_locals(&mut out, &snap, 0, 12, &ctx).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("@"), "no span → no provenance: {text}");
        // Out-of-range span → also omitted (degrades gracefully).
        let snap2 = snap_with_locals(
            "main",
            vec![local(
                "",
                0,
                praxis_runtime::LOCAL_KIND_TEMP,
                Some((100, 200)),
                0,
            )],
        );
        let ctx2 = RenderCtx {
            db: None,
            source_text: Some("short"),
        };
        let mut out = Vec::new();
        render_frame_locals(&mut out, &snap2, 0, 12, &ctx2).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            !text.contains("@"),
            "out-of-range span → no provenance: {text}"
        );
    }

    #[test]
    fn limit_caps_total_locals_users_first() {
        // 2 users + 3 temps (each visible via a span), limit 3 → both users + 1
        // temp shown, 2 more noted.
        let snap = snap_with_locals(
            "main",
            vec![
                local("a", 0, praxis_runtime::LOCAL_KIND_USER, None, 0),
                local("b", 1, praxis_runtime::LOCAL_KIND_USER, None, 0),
                local("", 2, praxis_runtime::LOCAL_KIND_TEMP, Some((0, 1)), 0),
                local("", 3, praxis_runtime::LOCAL_KIND_TEMP, Some((0, 1)), 0),
                local("", 4, praxis_runtime::LOCAL_KIND_TEMP, Some((0, 1)), 0),
            ],
        );
        let mut out = Vec::new();
        render_frame_locals(&mut out, &snap, 0, 3, &RenderCtx::bare()).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("a ="), "user a shown: {text}");
        assert!(text.contains("b ="), "user b shown: {text}");
        assert!(text.contains("<tmp#2>"), "one temp shown: {text}");
        assert!(
            !text.contains("<tmp#3>"),
            "limit caps temps after users: {text}"
        );
        assert!(text.contains("2 more"), "ellipsis notes remainder: {text}");
    }

    #[test]
    fn dead_scratch_temps_are_hidden() {
        // A temp with no value and no span is dead scratch (a closure's hidden
        // self arg after the capture prologue, or a pre-allocated return slot
        // the body never wrote): it holds nothing and explains nothing, so the
        // renderer drops it rather than show `<tmp#N> = <uninit>`.
        let snap = snap_with_locals(
            "main",
            vec![
                // Dead scratch: filtered out.
                local("", 0, praxis_runtime::LOCAL_KIND_TEMP, None, 0),
                // Live via a span (the faulting expression): kept, even though
                // its value is uninit.
                local("", 1, praxis_runtime::LOCAL_KIND_TEMP, Some((0, 5)), 0),
            ],
        );
        let mut out = Vec::new();
        render_frame_locals(&mut out, &snap, 0, 12, &RenderCtx::bare()).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            !text.contains("<tmp#0>"),
            "dead scratch temp is hidden: {text}"
        );
        assert!(
            text.contains("<tmp#1>"),
            "provenance-bearing temp is kept: {text}"
        );
    }

    #[test]
    fn temps_only_frame_shows_only_temps_section() {
        // No user locals → only the `temps:` section, no `locals:` header. The
        // temp carries a span so it is visible.
        let snap = snap_with_locals(
            "main",
            vec![local(
                "",
                0,
                praxis_runtime::LOCAL_KIND_TEMP,
                Some((0, 1)),
                0,
            )],
        );
        let mut out = Vec::new();
        render_frame_locals(&mut out, &snap, 0, 12, &RenderCtx::bare()).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("temps:"), "temps section: {text}");
        assert!(!text.contains("locals:"), "no empty locals header: {text}");
    }

    // ---- M10b-WS3: source/input/parser context rendering ----

    #[test]
    fn source_span_renders_lines_with_caret() {
        // A two-line source; the span covers "b + c" on line 2.
        let src = "fn f() {\n  a = b + c\n}\n";
        // "b + c" starts after "fn f() {\n  a = " = 13 bytes, length 5.
        let span = (13, 18);
        let mut out = Vec::new();
        render_source_span(&mut out, src, "f", span).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("f:"), "header: {text}");
        assert!(text.contains("a = b + c"), "the source line: {text}");
        // The caret must cover all 5 bytes of "b + c" and line up under the `|`
        // gutter — the shared snippet helper clamps it to the visible line.
        assert!(
            text.contains("    |     ^^^^^"),
            "caret at the right column covering the span: {text:?}"
        );
    }

    #[test]
    fn source_span_multiline_does_not_overflow() {
        // A span covering the whole `fn` (3 lines): each line is underlined to
        // its end and the caret never runs past the visible line. The earlier
        // implementation drew `end - start` carets all on line 1.
        let src = "fn f() {\n  a = b + c\n}\n";
        let span = (0, src.len() as u32);
        let mut out = Vec::new();
        render_source_span(&mut out, src, "f", span).unwrap();
        let text = String::from_utf8(out).unwrap();
        // Line 1's caret runs to the line end then stops (8 carets + ellipsis),
        // never reaching the width of the whole 3-line span.
        assert!(
            text.contains("    | ^^^^^^^^..."),
            "line 1 caret clamped to its line: {text:?}"
        );
        assert!(
            !text.contains("^^^^^^^^^^^^^^"),
            "no runaway caret spanning multiple lines: {text:?}"
        );
    }

    #[test]
    fn source_span_zero_span_shows_no_span_note() {
        let mut out = Vec::new();
        render_source_span(&mut out, "fn f() {}", "f", (0, 0)).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("no source span recorded"), "{text}");
    }

    #[test]
    fn source_span_out_of_range_shows_note() {
        let mut out = Vec::new();
        render_source_span(&mut out, "abc", "f", (10, 20)).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("outside the program source"), "{text}");
    }

    #[test]
    fn input_context_renders_parse_detail() {
        let mut detail = ParseDetail::new();
        use praxis_runtime::ParseFail;
        detail.consider(ParseFail::at(2, 1, "digit"), b"x9");
        detail.actual_preview = "x9".to_string();
        let mut out = Vec::new();
        render_input_context(&mut out, Some(&detail), "x9").unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("offset 2..3"), "span shown: {text}");
        assert!(text.contains("x9"), "preview shown: {text}");
    }

    #[test]
    fn input_context_no_detail_for_non_parse_fault() {
        let mut out = Vec::new();
        render_input_context(&mut out, None, "").unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("no input context available"), "{text}");
    }

    #[test]
    fn parser_context_renders_expected_and_expression() {
        let mut detail = ParseDetail::new();
        use praxis_runtime::ParseFail;
        // The parser expression span covers `int` in the source "read int".
        detail.consider(
            ParseFail::at(0, 0, "int").with_parser_span(Some((5, 8))),
            b"",
        );
        let mut out = Vec::new();
        render_parser_context(&mut out, Some(&detail), "read int").unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("expected: int"), "expected shown: {text}");
        assert!(text.contains("int"), "parser expression shown: {text}");
    }

    #[test]
    fn parser_context_unknown_when_no_parser_span() {
        let mut detail = ParseDetail::new();
        use praxis_runtime::ParseFail;
        detail.consider(ParseFail::here(0, "int"), b"");
        let mut out = Vec::new();
        render_parser_context(&mut out, Some(&detail), "").unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("<unknown parser>"), "{text}");
    }
}
