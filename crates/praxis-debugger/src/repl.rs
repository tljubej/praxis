//! The interactive crash REPL (§9.4) — and the breakpoint one (§9.8).
//!
//! When a fault fires and the host is attached to a terminal (or `--debug=always`
//! forces it), the CLI hands the crash snapshot to this REPL. The user navigates
//! the frame chain and inspects locals; **no command mutates or resumes the
//! faulted state** (§9.5, §19.10) — `restart`/`reload` rerun the program from the
//! start rather than continuing the faulted run.
//!
//! Commands (§9.4):
//! - `bt`              show the numbered backtrace
//! - `frame N`         select frame N
//! - `up`              move the selection toward the caller
//! - `down`            move the selection toward the callee
//! - `locals`          show the selected frame's locals
//! - `p EXPR`          evaluate a read-only expression
//! - `type EXPR`       show the inferred expression type
//! - `heap EXPR`       inspect a value with its type
//! - `source [N]`      show a frame's source
//! - `input`           show the input near the active parser cursor
//! - `parser`          show the active input parser near the fault
//! - `restart`         rerun the program with the same input
//! - `reload`          recompile the source and rerun with the same input
//! - `help`            list commands
//! - `quit` (or EOF)   exit the REPL
//!
//! ## The same engine serves a `:bp` stop
//!
//! A stop (§9.8) is the same questions asked of the same kind of snapshot, so it
//! is the same REPL rather than a second one — and what differs between the two
//! is *which commands are available*, which [`Attached`] settles once. A stopped
//! program has frames to return to, so it gains `continue`; it is in the middle
//! of using its own runtime, so it loses everything that would execute
//! (`p`/`heap`/`restart`/`reload`).
//!
//! That still leaves §9.5's rule exactly where it was. Nothing here resumes a
//! *faulted* state, and nothing here mutates any state at all: `continue` returns
//! a program that was never faulted to the point it was already at.

use std::io::{BufRead, Write};

use praxis_runtime::CrashSnapshot;

use crate::render::{
    RenderCtx, render_backtrace, render_frame_locals, render_input_context, render_parser_context,
    render_source_span,
};
use crate::session::DebugSession;

/// The prompt shown when the REPL is waiting for a command (§9.4).
pub const PROMPT: &str = "Praxis crash> ";

/// The prompt at a `:bp` stop (§9.8). Different from [`PROMPT`] because the
/// situation is: a crash prompt appears after a program has failed, and this one
/// appears in the middle of one that has not.
pub const STOP_PROMPT: &str = "Praxis stop> ";

/// What the REPL is attached to — which decides what its commands can do.
///
/// The three arms are three genuinely different situations, and separating them
/// is what keeps a command from being offered where it cannot work:
///
/// | arm | when | `p`/`heap` | `restart`/`reload` | `continue` |
/// |---|---|---|---|---|
/// | [`Nothing`](Attached::Nothing) | unit tests, a fault before any frame | no | no | no |
/// | [`Fault`](Attached::Fault) | a faulted run, fully unwound | yes | yes | no |
/// | [`Stopped`](Attached::Stopped) | a `:bp` stop, frames still live | no | no | **yes** |
///
/// The two "no"s in the `Stopped` row are the same fact twice. A stop happens
/// *underneath* the program's own frames: they are still claimed on the shadow
/// and debug stacks, and the host's `Runtime`, `Jit` and `Analysis` are borrowed
/// by the call that is running. There is no owned session to reach, and minting
/// a second `RuntimeContext` to evaluate against would start with the full stack
/// budget while the native stack is already deep — which is the one thing
/// [`Runtime::context`](praxis_runtime::Runtime::context)'s contract rules out.
/// So what a stop is lent is what can be *read*: the type database and the
/// source text.
enum Attached {
    /// No live state: navigation and nothing else.
    Nothing,
    /// A faulted run whose frames have all unwound, so the REPL owns the whole
    /// compile/run state and every command works.
    Fault(Box<DebugSession>),
    /// A run stopped at a `:bp` marker with its frames still live.
    ///
    /// Boxed for [`Fault`](Self::Fault)'s reason: the host owns a whole
    /// `TypeDb`, so the variant is two orders of magnitude larger than the
    /// other two and every `Attached` — including the `Nothing` the REPL sits
    /// in most of the time — would be sized for it.
    Stopped(Box<StoppedHost>),
}

/// What the host lends the debugger for the duration of a `:bp` stop.
///
/// Owned rather than borrowed, because the borrow it would otherwise need does
/// not exist to be taken: the handler is called from generated code, several
/// native frames below the host that owns the real `TypeDb` and source text.
/// Both are cheap beside a program run — a `TypeDb` is an append-only arena of
/// interned types, and a source file is a source file — and the clone is taken
/// once per run, not once per stop.
///
/// The `TypeDb` **must** be a copy of the one codegen used and never a fresh
/// one: a [`DebugLocal`](praxis_runtime::DebugLocal)'s `type_id` is positional,
/// so the same index in another db names another type. A clone preserves every
/// index, which is what makes it a legal stand-in where a fresh db is not.
pub struct StoppedHost {
    /// The program's type database, for rendering each local's static type and
    /// for answering `type EXPR`.
    pub db: praxis_typeck::TypeDb,
    /// The program's source text, for `source` and for each temp's `@ "expr"`
    /// provenance.
    pub source_text: String,
    /// The program's file name, for the source pane's title.
    pub source_name: String,
    /// Which stop this is, counting from 1 — [`BreakpointStop::hits`]. The
    /// banner says it, because a marker in a loop is the common case and "the
    /// third time round" is the first thing you want to know about a stop.
    ///
    /// [`BreakpointStop::hits`]: praxis_runtime::BreakpointStop::hits
    pub hits: u64,
    /// The `:bp` marker's own source span. The stopped frame's line is *known*
    /// here, where a faulted frame's has to be inferred from its unfinished
    /// temps ([`crate::tui`]'s `fault_span`) — so the source pane points at the
    /// marker rather than guessing.
    pub span: (u32, u32),
}

/// The interactive crash REPL. Owns the snapshot (taken from the runtime by the
/// host), the selected frame index, and whatever live state the host attached —
/// see [`Attached`].
///
/// The attachment is an enum rather than an `Option<DebugSession>` so that pure-
/// navigation unit tests can build a synthetic snapshot directly (`Repl::new`)
/// without standing up a whole compile/run pipeline, *and* so a `:bp` stop can
/// be given the read-only half of a session without being given the half that
/// would execute code. The CLI's two paths are [`Repl::new_session`] and
/// [`Repl::new_stopped`].
pub struct Repl {
    snapshot: CrashSnapshot,
    selected: usize,
    attached: Attached,
}

impl Repl {
    /// Construct a navigation-only REPL over `snapshot` (no live state).
    /// Used by unit tests and by the (rare) case where a fault fired before
    /// any debug frame, leaving the host with just a snapshot to render.
    /// Commands that need more (`p`, `source`, `restart`, …) print a
    /// "not available" note.
    #[must_use]
    pub fn new(snapshot: CrashSnapshot) -> Self {
        Repl {
            snapshot,
            selected: 0,
            attached: Attached::Nothing,
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
            attached: Attached::Fault(Box::new(session)),
        }
    }

    /// Construct a REPL over a `:bp` stop's `snapshot`, with the read-only
    /// state `host` lent it for the duration (§9.8). This is the CLI's
    /// breakpoint-handler path.
    #[must_use]
    pub fn new_stopped(snapshot: CrashSnapshot, host: StoppedHost) -> Self {
        Repl {
            snapshot,
            selected: 0,
            attached: Attached::Stopped(Box::new(host)),
        }
    }

    /// Consume the REPL and hand back the live session, dropping the snapshot
    /// first.
    ///
    /// The order is the point. A `CrashSnapshot` holds `GcRef`s into the
    /// session's heap, and the heap finalizes its payloads on drop — so a
    /// snapshot outliving the runtime is a use-after-free waiting to be read.
    /// Destructuring here makes that explicit instead of relying on field
    /// declaration order surviving the next edit. The caller gets a session it
    /// can [`DebugSession::teardown`].
    ///
    /// A REPL that was stopped at a `:bp` has no session to give back: the
    /// program owns its runtime and is about to go on using it.
    #[must_use]
    pub fn into_session(self) -> Option<DebugSession> {
        let Repl {
            snapshot, attached, ..
        } = self;
        drop(snapshot);
        match attached {
            Attached::Fault(session) => Some(*session),
            Attached::Nothing | Attached::Stopped(_) => None,
        }
    }

    /// Borrow the live session, if any. Commands that need to *execute*
    /// (`p EXPR`, `restart`) use this; it is `None` both for a navigation-only
    /// REPL and for a `:bp` stop, which has state to read but nothing to run.
    pub fn session(&self) -> Option<&DebugSession> {
        match &self.attached {
            Attached::Fault(session) => Some(session),
            Attached::Nothing | Attached::Stopped(_) => None,
        }
    }

    /// Whether this REPL is sitting on a live program that can be resumed —
    /// i.e. whether `continue` means anything here.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        matches!(self.attached, Attached::Stopped(_))
    }

    /// Which stop this is, counting from 1, or `None` if this is not one.
    #[must_use]
    pub fn stop_hits(&self) -> Option<u64> {
        match &self.attached {
            Attached::Stopped(host) => Some(host.hits),
            Attached::Nothing | Attached::Fault(_) => None,
        }
    }

    /// The `:bp` marker's span, or `None` if this is not a stop. Frame 0's line,
    /// exactly, rather than the inference a fault needs.
    #[must_use]
    pub fn stop_span(&self) -> Option<(u32, u32)> {
        match &self.attached {
            Attached::Stopped(host) => Some(host.span),
            Attached::Nothing | Attached::Fault(_) => None,
        }
    }

    /// The program source text, from whichever attachment has it.
    ///
    /// The one accessor both the `source` command and the TUI's source and
    /// backtrace panes read, so a stop and a fault cannot render the same
    /// program differently.
    #[must_use]
    pub fn source_text(&self) -> Option<&str> {
        match &self.attached {
            Attached::Fault(session) => Some(session.source_text.as_str()),
            Attached::Stopped(host) => Some(host.source_text.as_str()),
            Attached::Nothing => None,
        }
    }

    /// The program's file name, for a pane title. Empty when unknown.
    #[must_use]
    pub fn source_name(&self) -> &str {
        match &self.attached {
            Attached::Fault(session) => session
                .source_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(""),
            Attached::Stopped(host) => host.source_name.as_str(),
            Attached::Nothing => "",
        }
    }

    /// The rendering context for `locals` — the type database and the source
    /// text, from whichever attachment has them.
    #[must_use]
    pub fn render_ctx(&self) -> RenderCtx<'_> {
        match &self.attached {
            Attached::Fault(session) => RenderCtx::new(&session.analysis.db, &session.source_text),
            Attached::Stopped(host) => RenderCtx::new(&host.db, &host.source_text),
            Attached::Nothing => RenderCtx::bare(),
        }
    }

    /// Borrow the crash snapshot. The rendering commands (`source`,
    /// `render_frame_locals`) read frame spans/locals from it.
    pub fn snapshot(&self) -> &CrashSnapshot {
        &self.snapshot
    }

    /// The currently selected frame index (0 = innermost = faulting function).
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Select frame `index`, ignoring an index past the end of the chain.
    ///
    /// This is the `frame N` command's effect without its output, for the TUI's
    /// keyboard navigation: a keypress moves the cursor and the panes redraw, so
    /// there is no line to print. Out-of-range is silently ignored rather than
    /// reported, because the only caller clamps to the chain first — the guard is
    /// here so that a future one cannot desynchronize `selected` from `frames`.
    pub fn select(&mut self, index: usize) {
        if index < self.snapshot.len() {
            self.selected = index;
        }
    }

    /// The command list for whichever surface this is.
    fn help_text(&self) -> &'static str {
        if self.is_stopped() {
            STOPPED_HELP_TEXT
        } else {
            HELP_TEXT
        }
    }

    /// Borrow the live session mutably, if there is one.
    fn session_mut(&mut self) -> Option<&mut DebugSession> {
        match &mut self.attached {
            Attached::Fault(session) => Some(session),
            Attached::Nothing | Attached::Stopped(_) => None,
        }
    }

    /// Run `p EXPR` / `type EXPR` against the selected frame (§9.5).
    /// Splits the snapshot/frame borrow (immutable) from the session borrow
    /// (mutable: the runtime hosts the call) so both coexist. Degrades to a
    /// "session not attached" error for navigation-only REPLs, and to `type`
    /// alone for a `:bp` stop.
    fn evaluate_expr(
        &mut self,
        expr: &str,
        mode: crate::evaluate::Mode,
    ) -> crate::evaluate::EvalResult {
        // `type EXPR` infers against the selected frame's locals and runs no
        // code, so a `:bp` stop can serve it out of the lent `TypeDb` — which
        // is the whole of what makes it available where `p` is not.
        // Destructured rather than field-accessed, so the snapshot's shared
        // borrow and the attachment's mutable one are two borrows of two fields
        // instead of two of `self`.
        let Repl {
            snapshot,
            selected,
            attached,
        } = self;
        let frame = match snapshot.frames.get(*selected) {
            Some(f) => f,
            None => return Err("no frame selected".to_string()),
        };
        let session = match attached {
            Attached::Stopped(host) => {
                return match mode {
                    crate::evaluate::Mode::Type => {
                        crate::evaluate::type_of(&mut host.db, frame, expr)
                    }
                    crate::evaluate::Mode::Print | crate::evaluate::Mode::Heap => Err(
                        "the program is stopped, not faulted — evaluating an expression \
                         would run code underneath its live frames. `type EXPR` gives the \
                         static type; `continue` gives back a program the crash debugger \
                         can evaluate against."
                            .to_string(),
                    ),
                };
            }
            Attached::Fault(session) => session,
            Attached::Nothing => {
                return Err("no live session — cannot evaluate expressions".to_string());
            }
        };
        // Clone the `Rc` before the `&mut` borrows below: the evaluation
        // generation is a session field like the others.
        let generation = std::rc::Rc::clone(&session.eval_generation);
        let db = &mut session.analysis.db;
        match mode {
            crate::evaluate::Mode::Print => crate::evaluate::evaluate(
                db,
                &mut session.runtime,
                snapshot,
                frame,
                expr,
                &generation,
            ),
            crate::evaluate::Mode::Type => crate::evaluate::type_of(db, frame, expr),
            crate::evaluate::Mode::Heap => {
                crate::evaluate::heap(db, &mut session.runtime, snapshot, frame, expr, &generation)
            }
        }
    }

    /// Run `restart` or `reload` (§9.7), then update the REPL state: take the
    /// new snapshot (replacing `self.snapshot`), reset the frame cursor, and
    /// print the result or the new fault. A clean run prints the result and
    /// stays in the REPL. A failed `reload` (compile error) leaves the session
    /// intact and prints the error.
    fn do_restart_or_reload<O: Write>(&mut self, out: &mut O, reload: bool) {
        if self.is_stopped() {
            let _ = writeln!(
                out,
                "error: the program is stopped, not faulted — it has live frames to return \
                 to. `continue` runs it to its end (or its next stop); a restart is what \
                 the crash debugger offers after that."
            );
            return;
        }
        // Nothing arms `:bp` stops here, and that is deliberate: this re-runs the
        // program from inside the debugger's own screen, and a stop handler that
        // took over the terminal would be taking it from the debugger already
        // holding it. The host disarms stops before handing a fault off (§9.8);
        // a run that wants them is a fresh `praxis run`.
        let Some(session) = self.session_mut() else {
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
    /// output + the prompt to `output`. Each command's output is also returned
    /// for testing via [`Repl::handle`] (this method drives the loop; tests call
    /// `handle`).
    ///
    /// Returns **how the loop ended**, which the caller of a `:bp` stop needs:
    /// [`Control::Resume`] means let the program run on, [`Control::Quit`] means
    /// let it run on and stop at nothing else. A crash REPL's caller has one
    /// thing to do either way and can ignore the answer.
    pub fn run<I, O>(&mut self, input: &mut I, output: &mut O) -> Control
    where
        I: BufRead,
        O: Write,
    {
        if self.snapshot.is_empty() {
            let _ = writeln!(output, "(no frames to inspect)");
            // A stop with nothing to show still has a program to give back, and
            // `Quit` would detach it — a worse answer than resuming for a state
            // nothing put the user in. Every generated prologue claims a frame,
            // so this is a guard rather than a case.
            return if self.is_stopped() {
                Control::Resume
            } else {
                Control::Quit
            };
        }
        if let Some(hits) = self.stop_hits() {
            let ordinal = if hits > 1 {
                format!(" (stop #{hits})")
            } else {
                String::new()
            };
            let _ = writeln!(
                output,
                "Stopped at a breakpoint{ordinal}. {} frame(s). `continue` resumes; \
                 `help` lists commands.",
                self.snapshot.len()
            );
        } else {
            let _ = writeln!(
                output,
                "Entered crash debugger. {} frame(s). Type `help` for commands.",
                self.snapshot.len()
            );
        }
        let prompt = if self.is_stopped() {
            STOP_PROMPT
        } else {
            PROMPT
        };
        let mut line = String::new();
        loop {
            line.clear();
            let _ = write!(output, "{prompt}");
            let _ = output.flush();
            match input.read_line(&mut line) {
                // EOF. A stopped program still has to be let go of, and there is
                // no one left to ask, so the answer is the one that does not hang
                // a pipeline: resume.
                Ok(0) => return Control::Resume,
                Ok(_) => {
                    let cmd = line.trim();
                    if cmd.is_empty() {
                        continue;
                    }
                    match self.handle(cmd, output) {
                        Control::Continue => {}
                        end => return end,
                    }
                }
                Err(_) => return Control::Resume,
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
                let ctx = self.render_ctx();
                let _ = render_frame_locals(out, &self.snapshot, self.selected, usize::MAX, &ctx);
            }
            "help" | "?" => {
                let _ = writeln!(out, "{}", self.help_text());
            }
            // `continue` is the one command a `:bp` stop has and a fault does
            // not, and the asymmetry is the fault model's (§9.1): a faulted
            // program has no frames left to return to, so there is nothing a
            // resume could resume. Saying that here is worth more than an
            // "unknown command".
            "continue" | "cont" | "c" => {
                if !self.is_stopped() {
                    let _ = writeln!(
                        out,
                        "error: nothing to continue — this program faulted and its frames \
                         have unwound. `restart` reruns it from the beginning."
                    );
                    return Control::Continue;
                }
                let _ = writeln!(out, "continuing.");
                return Control::Resume;
            }
            // At a stop, `quit` cannot end the program — §9.2 forbids unwinding
            // Rust through JIT frames, so there is no way back out of the middle
            // of a call. What it can do is stop *debugging*: the program runs to
            // its end and no later marker takes the terminal again.
            "quit" | "exit" | "q" => {
                if self.is_stopped() {
                    let _ = writeln!(
                        out,
                        "leaving the debugger; the program runs on and will not stop again."
                    );
                }
                return Control::Quit;
            }
            // Context commands. `source` reads the selected frame's
            // `source_span` against the session's source text; `input`/`parser`
            // read the runtime's §7.11 `ParseDetail`.
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
                match self.source_text() {
                    Some(text) => {
                        let _ = render_source_span(out, text, name, span);
                    }
                    None => {
                        let _ = writeln!(out, "(no source available — session not attached)");
                    }
                }
            }
            // The §7.11 parse detail lives on the runtime's own slot, which a
            // stop is not lent — so these two answer "nothing recorded" there,
            // which is also the truth for a program that has not failed a parse.
            "input" => {
                let detail = self.session().map(|s| s.runtime.parse_detail());
                let input_text = self.session().map(|s| s.input_text.as_str()).unwrap_or("");
                let _ = render_input_context(out, detail, input_text);
            }
            "parser" => {
                let detail = self.session().map(|s| s.runtime.parse_detail());
                let source_text = self.source_text().unwrap_or("");
                let _ = render_parser_context(out, detail, source_text);
            }
            // The read-only expression evaluator (§9.4, §9.5). The three
            // commands differ only in the mode they hand `evaluate_expr`;
            // [`crate::evaluate::Mode`] documents what each one does with the
            // synthesized function and owns the word → variant mapping.
            "p" | "type" | "heap" => {
                if rest.is_empty() {
                    let _ = writeln!(out, "usage: {cmd} EXPR");
                    return Control::Continue;
                }
                let mode = crate::evaluate::Mode::from_command(cmd)
                    .expect("this arm's pattern lists exactly `Mode::from_command`'s words");
                let result = self.evaluate_expr(rest, mode);
                let _ = crate::evaluate::write_eval_result(out, &result);
            }
            // `restart` reruns the same code+input; `reload` recompiles the
            // source then reruns (§9.7). Both take the new snapshot (if it
            // faulted) and reset the frame cursor; a clean run prints the
            // result and stays in the REPL.
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

/// What a command asked the surface driving it to do next.
///
/// Three arms rather than two, because a `:bp` stop ends in a way a crash REPL
/// cannot: the program is still there, and letting go of it is not the same
/// decision as being done with the debugger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    /// Read another command.
    Continue,
    /// Leave the debugger. From a stop, the program runs on and stops at nothing
    /// else this run; from a fault, there is nothing left to run.
    Quit,
    /// Let the stopped program run on — and stop again at the next marker.
    Resume,
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
Crash debugger commands:
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

/// The `help` text at a `:bp` stop (§9.8).
///
/// A separate list rather than the one above with notes appended, because the
/// difference is not a footnote: the commands that would *execute* something are
/// absent for the reason [`Attached`] states, and `continue` is here instead.
/// Listing a command the surface will refuse is how a user learns to distrust
/// the help.
const STOPPED_HELP_TEXT: &str = "\
Breakpoint commands:
  continue        let the program run on (stops again at the next `:bp`)
  bt              show the numbered backtrace
  frame N         select frame N
  up              move the selection toward the caller
  down            move the selection toward the callee
  locals          show the selected frame's locals
  type EXPR       show the inferred expression type
  source [N]      show the selected (or Nth) frame's source
  help            show this message
  quit            run on without stopping again

`p EXPR` and `restart` need a program whose frames have unwound; this one is
still in the middle of its own. Continue, and the crash debugger has both.";

#[cfg(test)]
mod tests {
    use super::*;
    use praxis_runtime::{FaultKind, crash_snapshot::SnapshotFrame};

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
        // Commands that need the session degrade gracefully off this.
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

    // -----------------------------------------------------------------------
    // `:bp` stops (§9.8)
    // -----------------------------------------------------------------------

    /// A REPL over a stop, with the read-only state a host lends one.
    fn stopped_repl() -> Repl {
        Repl::new_stopped(
            two_frame_snapshot(),
            StoppedHost {
                db: praxis_typeck::TypeDb::new(),
                source_text: "var a = 1 :bp\n".to_string(),
                source_name: "stop.px".to_string(),
                hits: 1,
                span: (10, 13),
            },
        )
    }

    /// `continue` is the one command a stop has and a fault does not, and the
    /// answer it gives the surface is what resumes the program.
    #[test]
    fn continue_resumes_a_stop_and_says_so_at_a_fault() {
        let mut repl = stopped_repl();
        let mut out = Vec::new();
        for word in ["continue", "cont", "c"] {
            out.clear();
            assert_eq!(repl.handle(word, &mut out), Control::Resume, "`{word}`");
        }

        // At a fault there is nothing to resume, and the refusal says why rather
        // than pretending the command does not exist.
        let mut repl = Repl::new(two_frame_snapshot());
        let mut out = Vec::new();
        assert_eq!(repl.handle("continue", &mut out), Control::Continue);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("nothing to continue"), "{text}");
        assert!(text.contains("restart"), "it names what does work: {text}");
    }

    /// `quit` at a stop is not `continue`: it lets the program run on *and*
    /// stops at nothing else, which is a different answer for the host.
    #[test]
    fn quit_at_a_stop_is_distinct_from_continue() {
        let mut repl = stopped_repl();
        let mut out = Vec::new();
        assert_eq!(repl.handle("quit", &mut out), Control::Quit);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("will not stop again"), "{text}");
    }

    /// The commands a stop cannot serve refuse with a reason, and the ones it
    /// can still work — including `type`, which runs no code.
    #[test]
    fn a_stop_refuses_what_would_execute_and_serves_what_reads() {
        let mut repl = stopped_repl();
        for cmd in ["p 1 + 1", "heap 1"] {
            let mut out = Vec::new();
            repl.handle(cmd, &mut out);
            let text = String::from_utf8(out).unwrap();
            assert!(
                text.contains("stopped, not faulted"),
                "`{cmd}` explains itself: {text}"
            );
        }
        for cmd in ["restart", "reload"] {
            let mut out = Vec::new();
            repl.handle(cmd, &mut out);
            let text = String::from_utf8(out).unwrap();
            assert!(
                text.contains("live frames to return to"),
                "`{cmd}` explains itself: {text}"
            );
        }
        // Navigation is untouched, and `source` has the text the host lent.
        let mut out = Vec::new();
        repl.handle("bt", &mut out);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("boom") && text.contains("main"), "{text}");
        let mut out = Vec::new();
        repl.handle("source", &mut out);
        let text = String::from_utf8(out).unwrap();
        assert!(
            !text.contains("session not attached"),
            "a stop is lent its source: {text}"
        );
    }

    /// The help a stop prints is the stop's, not the fault's — listing a command
    /// the surface will refuse is how a user learns to distrust the help.
    #[test]
    fn a_stops_help_lists_continue_and_not_restart() {
        let mut repl = stopped_repl();
        let mut out = Vec::new();
        repl.handle("help", &mut out);
        let text = String::from_utf8(out).unwrap();
        // The command *list* is the indented block; the paragraph after it names
        // `p` and `restart` on purpose, to say where they went.
        let listed: String = text
            .lines()
            .filter(|l| l.starts_with("  "))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(listed.contains("continue"), "{text}");
        for absent in ["p EXPR", "restart", "reload", "input"] {
            assert!(
                !listed.contains(absent),
                "a stop cannot `{absent}`, so it is not offered: {text}"
            );
        }
        assert!(
            text.contains("still in the middle of its own"),
            "the missing commands are explained, not just absent: {text}"
        );
        // …and the fault's help is unchanged.
        let mut repl = Repl::new(two_frame_snapshot());
        let mut out = Vec::new();
        repl.handle("help", &mut out);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("restart"), "{text}");
        assert!(!text.contains("continue"), "{text}");
    }

    /// The stop loop ends on `continue`, on `quit`, and on EOF — and EOF resumes
    /// rather than detaching, because a pipe that ran out is not a user who
    /// asked to be left alone.
    #[test]
    fn the_stop_loop_ends_the_three_ways_it_can() {
        let cases = [
            (&b"continue\n"[..], Control::Resume),
            (&b"quit\n"[..], Control::Quit),
            (&b""[..], Control::Resume),
        ];
        for (bytes, want) in cases {
            let mut repl = stopped_repl();
            let mut input = bytes;
            let mut output = Vec::new();
            assert_eq!(repl.run(&mut input, &mut output), want);
        }
    }

    /// The banner numbers a stop past the first, because a marker in a loop is
    /// the common case.
    #[test]
    fn the_stop_banner_numbers_repeat_stops() {
        let mut repl = stopped_repl();
        let mut output = Vec::new();
        let _ = repl.run(&mut std::io::empty(), &mut output);
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("Stopped at a breakpoint."), "{text}");
        assert!(
            !text.contains("stop #"),
            "the first stop is not numbered: {text}"
        );

        let mut repl = Repl::new_stopped(
            two_frame_snapshot(),
            StoppedHost {
                db: praxis_typeck::TypeDb::new(),
                source_text: String::new(),
                source_name: String::new(),
                hits: 7,
                span: (0, 0),
            },
        );
        let mut output = Vec::new();
        let _ = repl.run(&mut std::io::empty(), &mut output);
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("(stop #7)"), "{text}");
    }

    /// A stop has no session to hand back: the program owns its runtime and is
    /// about to go on using it.
    #[test]
    fn a_stop_hands_back_no_session() {
        let repl = stopped_repl();
        assert!(repl.is_stopped());
        assert!(repl.session().is_none(), "nothing to execute against");
        assert_eq!(repl.source_name(), "stop.px");
        assert!(repl.into_session().is_none());
    }
}
