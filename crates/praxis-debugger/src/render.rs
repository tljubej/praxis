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

use praxis_runtime::{CrashSnapshot, FaultKind, ParseDetail};

/// The maximum number of locals the noninteractive fallback prints per frame
/// (§9.6 "up to configured limits"). Keeps the output a useful glance, not a
/// dump; a frame with more locals shows an ellipsis.
const NONINTERACTIVE_LOCAL_LIMIT: usize = 12;

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
) -> std::io::Result<()> {
    // 1. The fault line.
    writeln!(out, "error: program faulted: {kind}")?;

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
    render_frame_locals(out, snap, 0, NONINTERACTIVE_LOCAL_LIMIT)?;
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

/// Render the locals of frame `index` (0 = innermost), up to `limit`, as
/// `name = value` lines. Values are formatted through their descriptors. Locals
/// whose value is the null sentinel (not yet written) are shown as `<uninit>`.
/// Used by the noninteractive fallback and the REPL `locals`.
///
/// # Safety
/// The frame's locals and their descriptors must be valid (the snapshot was
/// deep-copied from a live chain; descriptors are compiler-embedded 'static).
pub fn render_frame_locals<W: Write>(
    out: &mut W,
    snap: &CrashSnapshot,
    index: usize,
    limit: usize,
) -> std::io::Result<()> {
    let Some(frame) = snap.frames.get(index) else {
        writeln!(out, "  <no frame {index}>")?;
        return Ok(());
    };
    if frame.locals.is_empty() {
        writeln!(out, "  (no locals in this frame)")?;
        return Ok(());
    }
    let total = frame.locals.len();
    let shown = total.min(limit);
    for local in &frame.locals[..shown] {
        let name = local.name();
        let display = if is_real_ref(local.value) {
            format_value(local.value)
        } else {
            "<uninit>".to_string()
        };
        writeln!(out, "  {name} = {display}")?;
    }
    if total > shown {
        writeln!(out, "  …({} more)", total - shown)?;
    }
    Ok(())
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
        };
        let mut s = CrashSnapshot::new();
        s.fault_kind = FaultKind::IndexOutOfBounds;
        s.frames = vec![frame];
        s
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
        render_noninteractive(&mut out, snap.fault_kind, Some(&snap), None).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("program faulted"), "fault line: {text}");
        assert!(text.contains("Backtrace"), "backtrace header: {text}");
        assert!(text.contains("boom"), "frame name: {text}");
    }

    #[test]
    fn noninteractive_without_snapshot_still_shows_fault() {
        let mut out = Vec::new();
        render_noninteractive(&mut out, FaultKind::DivByZero, None, None).unwrap();
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
        render_noninteractive(&mut out, FaultKind::ParseFailed, None, Some(&detail)).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("expected int"), "expected shown: {text}");
        assert!(text.contains("at input offset 5"), "offset shown: {text}");
    }

    #[test]
    fn empty_frame_locals_shows_no_locals_message() {
        let snap = snap_with_frame("main");
        let mut out = Vec::new();
        render_frame_locals(&mut out, &snap, 0, 12).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("no locals"), "{text}");
    }
}
