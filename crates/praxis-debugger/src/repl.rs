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

use crate::render::{render_backtrace, render_frame_locals};

/// The prompt shown when the REPL is waiting for a command (§9.4).
pub const PROMPT: &str = "Praxis crash> ";

/// The interactive crash REPL. Owns the snapshot (taken from the runtime by the
/// host), the selected frame index, and the I/O handles.
pub struct Repl {
    snapshot: CrashSnapshot,
    selected: usize,
}

impl Repl {
    /// Construct a REPL over `snapshot` (the host transfers ownership).
    #[must_use]
    pub fn new(snapshot: CrashSnapshot) -> Self {
        Repl {
            snapshot,
            selected: 0,
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
                let _ = render_frame_locals(out, &self.snapshot, self.selected, usize::MAX);
            }
            "help" | "?" => {
                let _ = writeln!(out, "{}", HELP_TEXT);
            }
            "quit" | "exit" | "q" => return Control::Quit,
            // M10b commands: acknowledged but not yet implemented.
            "p" | "type" | "source" | "input" | "parser" | "heap" | "restart" | "reload" => {
                let _ = writeln!(
                    out,
                    "note: `{cmd}` is implemented in Milestone 10 Part 2 (not yet wired)."
                );
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

/// The `help` text (§9.4 command list, M10a subset marked).
const HELP_TEXT: &str = "\
Crash debugger commands (§9.4):
  bt              show the numbered backtrace
  frame N         select frame N
  up              move the selection toward the caller
  down            move the selection toward the callee
  locals          show the selected frame's locals
  help            show this message
  quit            exit the debugger

Not yet wired (Milestone 10 Part 2):
  p EXPR, type EXPR, source, input, parser, heap, restart, reload";

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
        };
        let frame1 = SnapshotFrame {
            parent: usize::MAX,
            func_name: main.as_ptr(),
            func_name_len: main.len() as u32,
            locals: Vec::new(),
        };
        let mut s = CrashSnapshot::new();
        s.fault_kind = FaultKind::IndexOutOfBounds;
        s.frames = vec![frame0, frame1];
        s
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
