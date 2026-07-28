//! The interactive crash REPL (§9.4, M10-WS5).
//!
//! When a fault fires and the host is attached to a terminal (or `--debug=always`
//! forces it), the CLI hands the crash snapshot to this REPL. The user navigates
//! the frame chain and inspects locals; no command can mutate or resume the
//! faulted state in v1 (§9.5, §19.10 — `p EXPR` and the mutation gate land in
//! M10b).
//!
//! Commands (§9.4), M10a subset:
//! - `bt`              show the numbered backtrace
//! - `frame N`         select frame N
//! - `up`              move the selection toward the caller
//! - `down`            move the selection toward the callee
//! - `locals`          show the selected frame's locals
//! - `help`            list commands
//! - `quit` (or EOF)   exit the REPL
//!
//! `p EXPR`, `type EXPR`, `source`, `input`, `parser`, `heap`, `restart`,
//! `reload` are acknowledged but deferred to M10b.

use std::io::{BufRead, Write};

use praxis_runtime::CrashSnapshot;

use crate::render::{
    render_backtrace, render_frame_locals, render_input_context, render_parser_context,
    render_source_span, RenderCtx,
};
use crate::session::DebugSession;

/// The prompt shown when the REPL is waiting for a command (§9.4).
pub const PROMPT: &str = "Praxis crash> ";

/// The interactive crash REPL. Owns the snapshot (taken from the runtime by the
/// host), the selected frame index, and — when driven from a real fault — the
/// full [`DebugSession`] (live `Jit`/`Runtime`/`TypeDb`/source/input) the
/// `p EXPR`/`source`/`restart`/`reload` commands reach into.
///
/// The session is `Option`al so pure-navigation unit tests can build a
/// synthetic snapshot directly (`Repl::new`) without standing up a whole
/// compile/run pipeline. The CLI path uses [`Repl::new_session`].
pub struct Repl {
    snapshot: CrashSnapshot,
    selected: usize,
    session: Option<DebugSession>,
}

impl Repl {
    /// Construct a navigation-only REPL over `snapshot` (no live session).
    /// Used by unit tests and by the (rare) case where a fault fired before
    /// any debug frame, leaving the host with just a snapshot to render.
    /// Commands that need the session (`p`, `source`, `restart`, …) print a
    /// "not available" note.
    #[must_use]
    pub fn new(snapshot: CrashSnapshot) -> Self {
        Repl {
            snapshot,
            selected: 0,
            session: None,
        }
    }

    /// Construct a REPL over `snapshot` backed by the live `session`. The
    /// session's `Jit`/`Runtime`/`TypeDb`/source/input are now owned by the
    /// REPL and dropped when it is. This is the CLI's fault-handoff path.
    #[must_use]
    pub fn new_session(snapshot: CrashSnapshot, session: DebugSession) -> Self {
        Repl {
            snapshot,
            selected: 0,
            session: Some(session),
        }
    }

    /// Borrow the live session, if any. M10b commands use this to reach the
    /// `Jit`/`Runtime`/`TypeDb`; returns `None` for navigation-only REPLs.
    pub fn session(&self) -> Option<&DebugSession> {
        self.session.as_ref()
    }

    /// Mutably borrow the live session, if any. `restart`/`reload` mutate it.
    pub fn session_mut(&mut self) -> Option<&mut DebugSession> {
        self.session.as_mut()
    }

    /// Borrow the crash snapshot. M10b rendering commands (`source`,
    /// `render_frame_locals`) read frame spans/locals from it.
    pub fn snapshot(&self) -> &CrashSnapshot {
        &self.snapshot
    }

    /// The currently selected frame index (0 = innermost = faulting function).
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Run `p EXPR` / `type EXPR` against the selected frame (§9.5, M10b-WS4).
    /// Splits the snapshot/frame borrow (immutable) from the session borrow
    /// (mutable: the runtime hosts the call) so both coexist. Degrades to a
    /// "session not attached" error for navigation-only REPLs.
    fn evaluate_expr(
        &mut self,
        expr: &str,
        mode: crate::evaluate::Mode,
    ) -> crate::evaluate::EvalResult {
        let Some(session) = self.session.as_mut() else {
            return Err("no live session — cannot evaluate expressions".to_string());
        };
        // Borrow the snapshot + selected frame immutably, the session mutably.
        // The snapshot is `self.snapshot`, separate from `session`.
        let frame = match self.snapshot.frames.get(self.selected) {
            Some(f) => f,
            None => return Err("no frame selected".to_string()),
        };
        let db = &mut session.analysis.db;
        match mode {
            crate::evaluate::Mode::Print => {
                crate::evaluate::evaluate(db, &mut session.runtime, &self.snapshot, frame, expr)
            }
            crate::evaluate::Mode::Type => crate::evaluate::type_of(db, frame, expr),
            crate::evaluate::Mode::Heap => {
                crate::evaluate::heap(db, &mut session.runtime, &self.snapshot, frame, expr)
            }
        }
    }

    /// Run `restart` or `reload` (§9.7), then update the REPL state: take the
    /// new snapshot (replacing `self.snapshot`), reset the frame cursor, and
    /// print the result or the new fault. A clean run prints the result and
    /// stays in the REPL. A failed `reload` (compile error) leaves the session
    /// intact and prints the error.
    fn do_restart_or_reload<O: Write>(&mut self, out: &mut O, reload: bool) {
        let Some(session) = self.session.as_mut() else {
            let _ = writeln!(out, "error: no live session — cannot restart/reload");
            return;
        };
        // Run the re-execution, capturing the result GcRef (or an error for
        // a failed reload recompile).
        let result = if reload {
            session.reload()
        } else {
            Ok(session.restart())
        };
        match result {
            Ok(value) => {
                if session.runtime.has_pending_fault() {
                    let kind = session.runtime.fault();
                    // Take the new snapshot; the REPL owns it for inspection.
                    if let Some(snap) = session.runtime.take_crash_snapshot() {
                        self.snapshot = snap;
                        self.selected = 0;
                        let _ = writeln!(
                            out,
                            "program faulted: {kind}\n{} frame(s); frame 0 selected.",
                            self.snapshot.len()
                        );
                    } else {
                        let _ = writeln!(out, "program faulted: {kind} (no snapshot captured)");
                    }
                } else {
                    // Clean run: print the result, stay in the REPL.
                    let mut s = String::new();
                    value.format(&mut s);
                    let _ = writeln!(out, "program completed: {s}");
                }
            }
            Err(msg) => {
                let _ = writeln!(out, "error: {msg}");
                let _ = writeln!(
                    out,
                    "(session unchanged — the old snapshot is still active)"
                );
            }
        }
    }

    /// Run the read-eval-print loop, reading commands from `input` and writing
    /// output + the prompt to `output`. Returns when the user types `quit` or
    /// EOF is reached. Each command's output is also returned for testing via
    /// [`Repl::handle`] (this method drives the loop; tests call `handle`).
    pub fn run<I, O>(&mut self, input: &mut I, output: &mut O)
    where
        I: BufRead,
        O: Write,
    {
        if self.snapshot.is_empty() {
            let _ = writeln!(output, "(no crash snapshot to inspect)");
            return;
        }
        let _ = writeln!(
            output,
            "Entered crash debugger. {} frame(s). Type `help` for commands.",
            self.snapshot.len()
        );
        let mut line = String::new();
        loop {
            line.clear();
            let _ = write!(output, "{PROMPT}");
            let _ = output.flush();
            match input.read_line(&mut line) {
                Ok(0) => break, // EOF
                Ok(_) => {
                    let cmd = line.trim();
                    if cmd.is_empty() {
                        continue;
                    }
                    if self.handle(cmd, output) == Control::Quit {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    }

    /// Handle one command line, writing output to `out`. Returns whether the
    /// REPL should quit. Public so tests can drive single commands without a
    /// stdin loop.
    pub fn handle<O: Write>(&mut self, line: &str, out: &mut O) -> Control {
        let (cmd, rest) = split_cmd(line);
        match cmd {
            "bt" | "backtrace" => {
                let _ = render_backtrace(out, &self.snapshot);
                // Mark the selected frame.
                let _ = writeln!(out, "  (frame {} selected)", self.selected);
            }
            "frame" => match rest.parse::<usize>() {
                Ok(n) if n < self.snapshot.len() => {
                    self.selected = n;
                    // SAFETY: names are compiler-embedded 'static UTF-8.
                    let name = unsafe { self.snapshot.frame_name(n) };
                    let _ = writeln!(out, "frame {n}: {name}");
                }
                Ok(n) => {
                    let _ = writeln!(
                        out,
                        "error: frame {n} out of range (0..={})",
                        self.snapshot.len().saturating_sub(1)
                    );
                }
                Err(_) => {
                    let _ = writeln!(out, "usage: frame N");
                }
            },
            "up" => {
                if self.selected + 1 < self.snapshot.len() {
                    self.selected += 1;
                    // SAFETY: names are compiler-embedded 'static UTF-8.
                    let name = unsafe { self.snapshot.frame_name(self.selected) };
                    let _ = writeln!(out, "frame {}: {name}", self.selected);
                } else {
                    let _ = writeln!(out, "already at the outermost frame");
                }
            }
            "down" => {
                if self.selected > 0 {
                    self.selected -= 1;
                    // SAFETY: names are compiler-embedded 'static UTF-8.
                    let name = unsafe { self.snapshot.frame_name(self.selected) };
                    let _ = writeln!(out, "frame {}: {name}", self.selected);
                } else {
                    let _ = writeln!(out, "already at the innermost frame");
                }
            }
            "locals" => {
                let ctx = match self.session.as_ref() {
                    Some(s) => RenderCtx::new(&s.analysis.db, &s.source_text),
                    None => RenderCtx::bare(),
                };
                let _ = render_frame_locals(out, &self.snapshot, self.selected, usize::MAX, &ctx);
            }
            "help" | "?" => {
                let _ = writeln!(out, "{}", HELP_TEXT);
            }
            "quit" | "exit" | "q" => return Control::Quit,
            // M10b-WS3 context commands. `source` reads the selected frame's
            // source_span (threaded in WS1) against the session's source text.
            // `input`/`parser` read the runtime's §7.11 ParseDetail.
            "source" => {
                let frame_idx = match rest.parse::<usize>() {
                    Ok(n) if n < self.snapshot.len() => n,
                    _ => self.selected,
                };
                // SAFETY: names are compiler-embedded 'static UTF-8.
                let name = unsafe { self.snapshot.frame_name(frame_idx) };
                let span = self
                    .snapshot
                    .frames
                    .get(frame_idx)
                    .map(|f| f.source_span)
                    .unwrap_or((0, 0));
                match self.session.as_ref() {
                    Some(s) => {
                        let _ = render_source_span(out, &s.source_text, name, span);
                    }
                    None => {
                        let _ = writeln!(out, "(no source available — session not attached)");
                    }
                }
            }
            "input" => {
                let detail = self.session.as_ref().map(|s| s.runtime.parse_detail());
                let input_text = self
                    .session
                    .as_ref()
                    .map(|s| s.input_text.as_str())
                    .unwrap_or("");
                let _ = render_input_context(out, detail, input_text);
            }
            "parser" => {
                let detail = self.session.as_ref().map(|s| s.runtime.parse_detail());
                let source_text = self
                    .session
                    .as_ref()
                    .map(|s| s.source_text.as_str())
                    .unwrap_or("");
                let _ = render_parser_context(out, detail, source_text);
            }
            // M10b-WS4: the read-only `p EXPR` / `type EXPR` JIT evaluator
            // (§9.5). Synthesizes `fn __p_expr(<typed params>) { EXPR }`,
            // type-checks against the selected frame's locals, purity-gates,
            // JITs a fresh function, and calls it with the snapshot locals.
            "p" => {
                if rest.is_empty() {
                    let _ = writeln!(out, "usage: p EXPR");
                    return Control::Continue;
                }
                let result = self.evaluate_expr(rest, crate::evaluate::Mode::Print);
                let _ = crate::evaluate::write_eval_result(out, &result);
            }
            "type" => {
                if rest.is_empty() {
                    let _ = writeln!(out, "usage: type EXPR");
                    return Control::Continue;
                }
                let result = self.evaluate_expr(rest, crate::evaluate::Mode::Type);
                let _ = crate::evaluate::write_eval_result(out, &result);
            }
            // M10b-WS5: `heap EXPR` recursively inspects a value (§9.4). Like
            // `p EXPR` but prefixes the result with its type, so the structure
            // and type are visible at a glance.
            "heap" => {
                if rest.is_empty() {
                    let _ = writeln!(out, "usage: heap EXPR");
                    return Control::Continue;
                }
                let result = self.evaluate_expr(rest, crate::evaluate::Mode::Heap);
                let _ = crate::evaluate::write_eval_result(out, &result);
            }
            // M10b-WS6: `restart` reruns the same code+input; `reload`
            // recompiles the source then reruns (§9.7). Both take the new
            // snapshot (if it faulted) and reset the frame cursor; a clean run
            // prints the result and stays in the REPL.
            "restart" => {
                self.do_restart_or_reload(out, /* reload */ false);
            }
            "reload" => {
                self.do_restart_or_reload(out, /* reload */ true);
            }
            "" => {}
            other => {
                let _ = writeln!(out, "unknown command `{other}`. Type `help` for the list.");
            }
        }
        Control::Continue
    }
}

/// Whether the REPL should continue or quit after a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    Continue,
    Quit,
}

/// Split a command line into its leading word and the remainder.
fn split_cmd(line: &str) -> (&str, &str) {
    let trimmed = line.trim();
    match trimmed.split_once(char::is_whitespace) {
        Some((cmd, rest)) => (cmd, rest.trim()),
        None => (trimmed, ""),
    }
}

/// The `help` text (§9.4 command list).
const HELP_TEXT: &str = "\
Crash debugger commands (§9.4):
  bt              show the numbered backtrace
  frame N         select frame N
  up              move the selection toward the caller
  down            move the selection toward the callee
  locals          show the selected frame's locals
  p EXPR          evaluate a read-only expression
  type EXPR       show the inferred expression type
  heap EXPR       inspect a value with its type
  source [N]      show the selected (or Nth) frame's source
  input           show the input near the active parser cursor
  parser          show the active input parser near the fault
  restart         rerun the program with the same input
  reload          recompile source and rerun with the same input
  help            show this message
  quit            exit the debugger";

#[cfg(test)]
mod tests {
    use super::*;
    use praxis_runtime::{crash_snapshot::SnapshotFrame, FaultKind};

    /// Build a snapshot with two named frames (innermost `boom`, outer `main`)
    /// and no locals, for navigation tests.
    fn two_frame_snapshot() -> CrashSnapshot {
        let boom: &'static str = Box::leak("boom".to_string().into_boxed_str());
        let main: &'static str = Box::leak("main".to_string().into_boxed_str());
        let frame0 = SnapshotFrame {
            parent: 1,
            func_name: boom.as_ptr(),
            func_name_len: boom.len() as u32,
            locals: Vec::new(),
            source_span: (0, 0),
        };
        let frame1 = SnapshotFrame {
            parent: usize::MAX,
            func_name: main.as_ptr(),
            func_name_len: main.len() as u32,
            locals: Vec::new(),
            source_span: (0, 0),
        };
        let mut s = CrashSnapshot::new();
        s.fault_kind = FaultKind::IndexOutOfBounds;
        s.frames = vec![frame0, frame1];
        s
    }

    #[test]
    fn navigation_only_repl_has_no_session() {
        // `Repl::new` (the unit-test / degraded path) carries no live session:
        // `session()` is None, and the snapshot/selected accessors still work.
        // M10b commands that need the session will degrade gracefully off this.
        let repl = Repl::new(two_frame_snapshot());
        assert!(
            repl.session().is_none(),
            "navigation-only REPL has no session"
        );
        assert_eq!(repl.selected(), 0);
        assert_eq!(repl.snapshot().len(), 2);
    }

    #[test]
    fn bt_shows_both_frames_and_selection() {
        let mut repl = Repl::new(two_frame_snapshot());
        let mut out = Vec::new();
        repl.handle("bt", &mut out);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("#0"), "{text}");
        assert!(text.contains("#1"), "{text}");
        assert!(text.contains("boom"), "{text}");
        assert!(text.contains("main"), "{text}");
        assert!(text.contains("frame 0 selected"), "{text}");
    }

    #[test]
    fn frame_select_then_up_down() {
        let mut repl = Repl::new(two_frame_snapshot());
        let mut out = Vec::new();
        // Select frame 1 (main).
        repl.handle("frame 1", &mut out);
        assert_eq!(repl.selected, 1);
        // down → back to frame 0 (boom).
        out.clear();
        repl.handle("down", &mut out);
        assert_eq!(repl.selected, 0);
        let text = String::from_utf8(out.clone()).unwrap();
        assert!(text.contains("boom"), "{text}");
        // down again at innermost → error, stays at 0.
        out.clear();
        repl.handle("down", &mut out);
        assert_eq!(repl.selected, 0);
        let text = String::from_utf8(out.clone()).unwrap();
        assert!(text.contains("innermost"), "{text}");
        // up → frame 1.
        out.clear();
        repl.handle("up", &mut out);
        assert_eq!(repl.selected, 1);
        let text = String::from_utf8(out.clone()).unwrap();
        assert!(text.contains("main"), "{text}");
        // up again at outermost → error.
        out.clear();
        repl.handle("up", &mut out);
        assert_eq!(repl.selected, 1);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("outermost"), "{text}");
    }

    #[test]
    fn frame_out_of_range_errors() {
        let mut repl = Repl::new(two_frame_snapshot());
        let mut out = Vec::new();
        repl.handle("frame 99", &mut out);
        assert_eq!(repl.selected, 0, "selection unchanged on bad frame");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("out of range"), "{text}");
    }

    #[test]
    fn quit_returns_control_quit() {
        let mut repl = Repl::new(two_frame_snapshot());
        let mut out = Vec::new();
        assert_eq!(repl.handle("quit", &mut out), Control::Quit);
    }

    #[test]
    fn unknown_command_asks_for_help() {
        let mut repl = Repl::new(two_frame_snapshot());
        let mut out = Vec::new();
        repl.handle("frobnicate", &mut out);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("unknown command"), "{text}");
        assert!(text.contains("help"), "{text}");
    }

    #[test]
    fn help_lists_commands() {
        let mut repl = Repl::new(two_frame_snapshot());
        let mut out = Vec::new();
        repl.handle("help", &mut out);
        let text = String::from_utf8(out).unwrap();
        for cmd in ["bt", "frame", "up", "down", "locals", "help", "quit"] {
            assert!(text.contains(cmd), "help should list `{cmd}`: {text}");
        }
    }

    #[test]
    fn run_loop_quits_on_eof() {
        let mut repl = Repl::new(two_frame_snapshot());
        let mut input = std::io::empty();
        let mut output = Vec::new();
        repl.run(&mut input, &mut output);
        // EOF → loop exits immediately after printing the banner.
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("Entered crash debugger"), "{text}");
    }

    #[test]
    fn run_loop_processes_then_quits() {
        let snapshot = two_frame_snapshot();
        let mut repl = Repl::new(snapshot);
        let input_bytes = b"bt\nframe 1\nquit\n";
        let mut input = &input_bytes[..];
        let mut output = Vec::new();
        repl.run(&mut input, &mut output);
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("#1"), "bt ran: {text}");
        assert_eq!(repl.selected, 1, "frame 1 selected");
    }
}
