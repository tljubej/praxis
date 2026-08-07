//! The full-screen crash debugger (§9.4).
//!
//! [`crate::repl`] is the command engine: it owns the snapshot and the live
//! [`DebugSession`](crate::session::DebugSession), and `Repl::handle` turns one
//! command line into text. What it cannot do is show you a frame, its source and
//! its locals at the same time — a line-oriented REPL answers one question per
//! prompt, so navigating a fault means typing `up`, `locals`, `source`, `up`,
//! `locals`, `source`, and holding the results in your head.
//!
//! This module puts those answers on screen at once and makes the frame chain
//! something you move through with the arrow keys. It is a *view*, not a second
//! implementation: every command still runs through `Repl::handle`, so the two
//! surfaces cannot answer the same question differently, and the REPL's tests
//! remain the tests for command behavior.
//!
//! ```text
//!  ╭─ backtrace ──────────╮╭─ pick ───────────── boom.px ─╮
//!  │ ▶ #0  pick           ││   1  fn pick(xs: Vec[Int], … │
//!  │   #1  middle         ││   2      var scaled = i * 3  │
//!  │   #2  <entry>        ││ ▶ 3      return xs[scaled]   │
//!  ╰──────────────────────╯╰──────────────────────────────╯
//! ```
//!
//! ## Terminal ownership
//!
//! The TUI takes the alternate screen and raw mode, which means it owns the
//! terminal until it exits — including if it panics. [`run`] installs a panic
//! hook that restores the terminal first, because a panic in raw mode otherwise
//! leaves the user with a shell that does not echo what they type.
//!
//! It also requires a terminal to take. The caller decides: [`should_use_tui`]
//! is the check, and a `false` there means the line REPL runs instead. That is
//! not a lesser fallback for piped output — it is the correct surface for it.

use std::io::Write;

use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use praxis_runtime::{DebugLocal, FaultKind};

use crate::repl::{Control, Repl};

/// The palette. Named by role rather than by color so the whole scheme retunes
/// in one place, the same reasoning [`praxis_source::style::Style`] uses for
/// diagnostics.
///
/// Every color here is one of the 16 ANSI slots, never a fixed RGB value: those
/// slots are what the user's own terminal theme defines, so the debugger sits
/// inside whatever light or dark scheme they already chose instead of imposing a
/// palette that looks right in only one of them.
mod theme {
    use ratatui::style::Color;

    /// The fault, and the marker on the faulting line.
    pub const FAULT: Color = Color::Red;
    /// Pane borders and other structure the eye should skip.
    pub const CHROME: Color = Color::DarkGray;
    /// The border and title of the pane holding focus.
    pub const FOCUS: Color = Color::Cyan;
    /// A local's name; the selected frame's function.
    pub const NAME: Color = Color::Cyan;
    /// A type annotation.
    pub const TYPE: Color = Color::Yellow;
    /// A value.
    pub const VALUE: Color = Color::Green;
    /// Source line numbers, `<uninit>`, and other de-emphasized text.
    pub const MUTED: Color = Color::DarkGray;
    /// Keycap hints in the status bar.
    pub const KEY: Color = Color::Magenta;
}

/// Which pane the keyboard is driving. Navigation keys mean different things per
/// pane (frame selection vs. scrolling), so focus has to be explicit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Backtrace,
    Locals,
    Source,
    Output,
}

impl Focus {
    /// Tab order.
    fn next(self) -> Focus {
        match self {
            Focus::Backtrace => Focus::Locals,
            Focus::Locals => Focus::Source,
            Focus::Source => Focus::Output,
            Focus::Output => Focus::Backtrace,
        }
    }

    fn prev(self) -> Focus {
        match self {
            Focus::Backtrace => Focus::Output,
            Focus::Locals => Focus::Backtrace,
            Focus::Source => Focus::Locals,
            Focus::Output => Focus::Source,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Focus::Backtrace => "backtrace",
            Focus::Locals => "locals",
            Focus::Source => "source",
            Focus::Output => "output",
        }
    }
}

/// What the keyboard is doing: moving around, or typing a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Keys are bindings.
    Normal,
    /// Keys are text for the command line (entered with `:`).
    Command,
    /// The key-binding overlay is up (`?`); any key dismisses it.
    Help,
}

/// The debugger's screen state, wrapped around the [`Repl`] that owns the
/// snapshot and session.
pub struct Tui {
    repl: Repl,
    /// The fault that produced the snapshot, and any `panic`/`assert` message.
    /// The runtime's fault slot is cleared by a `restart`, so the banner reads
    /// from here rather than asking the session again.
    fault: FaultKind,
    fault_message: Option<String>,
    focus: Focus,
    mode: Mode,
    /// The command line being typed, and the cursor's byte offset into it.
    input: String,
    input_cursor: usize,
    /// Previously entered commands, newest last, walked with ↑/↓ in command mode.
    history: Vec<String>,
    /// Where in `history` the recall cursor sits; `None` when typing fresh text.
    history_pos: Option<usize>,
    /// Accumulated command output, newest last — the transcript a line REPL would
    /// have left on screen.
    output: Vec<String>,
    output_scroll: u16,
    /// The source pane's scroll, as an offset *from* the line the fault is on
    /// rather than from the top of the function.
    ///
    /// The pane shows the selected frame's whole extent, and a frame is a whole
    /// function — so anchoring at the top puts the marked line below the fold in
    /// anything longer than the pane, and finding the fault meant scrolling to it
    /// on every frame change. Storing a delta means 0 is "wherever the fault is",
    /// which is both the right default and the right thing to reset to.
    ///
    /// Signed because the user can scroll above the anchor; the resolved absolute
    /// offset is clamped to the document in [`source_anchor`].
    source_scroll: i32,
    locals_scroll: u16,
    /// Set by `quit` (or the command of the same name) to end the event loop.
    quit: bool,
}

impl Tui {
    /// Wrap `repl` in a screen. `fault`/`message` are the fault that produced its
    /// snapshot, for the banner.
    #[must_use]
    pub fn new(repl: Repl, fault: FaultKind, message: Option<String>) -> Tui {
        Tui {
            repl,
            fault,
            fault_message: message,
            focus: Focus::Backtrace,
            mode: Mode::Normal,
            input: String::new(),
            input_cursor: 0,
            history: Vec::new(),
            history_pos: None,
            output: vec![
                "Type `:` to run a command, `?` for keys.".to_string(),
                "↑↓ or j/k select a frame; u/d walk the call stack.".to_string(),
            ],
            output_scroll: 0,
            source_scroll: 0,
            locals_scroll: 0,
            quit: false,
        }
    }

    /// Consume the TUI and hand back the [`Repl`], so the caller can reach the
    /// session for its ordered teardown (F13, H15 — see
    /// [`crate::session::DebugSession::teardown`]).
    #[must_use]
    pub fn into_repl(self) -> Repl {
        self.repl
    }

    /// Run one command through [`Repl::handle`] and append its output.
    ///
    /// The command engine writes to a `Write`, so a `Vec<u8>` is all it takes to
    /// put a REPL command's output into a pane. This is what keeps the two
    /// surfaces from drifting: there is no second implementation of `locals` here.
    fn run_command(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        // Echo the command, so the transcript reads like a session.
        self.output.push(format!("❯ {line}"));
        let mut buf: Vec<u8> = Vec::new();
        let control = self.repl.handle(line, &mut buf);
        let text = String::from_utf8_lossy(&buf);
        for l in text.lines() {
            self.output.push(l.to_string());
        }
        if control == Control::Quit {
            self.quit = true;
        }
        // A command may have moved the frame cursor (`frame N`, `up`, `down`) or
        // replaced the snapshot entirely (`restart`, `reload`); either way the
        // source and locals panes are showing the wrong frame's scroll offset.
        self.source_scroll = 0;
        self.locals_scroll = 0;
        // Pin the transcript to its newest line: output that scrolls off the
        // moment it arrives is output the user never sees.
        self.scroll_output_to_end();
    }

    /// Park the output scroll at the last line.
    fn scroll_output_to_end(&mut self) {
        self.output_scroll = u16::try_from(self.output.len().saturating_sub(1)).unwrap_or(u16::MAX);
    }

    /// Move the frame selection by `delta`, clamped to the chain. Returns whether
    /// it moved.
    ///
    /// The add saturates because `Home`/`End` pass `isize::MAX`/`MIN` to mean
    /// "as far as it goes" — a wrapping add would panic in a debug build, which
    /// is every build the corpus tests run.
    fn move_frame(&mut self, delta: isize) -> bool {
        let len = self.repl.snapshot().len();
        if len == 0 {
            return false;
        }
        let current = self.repl.selected() as isize;
        let next = current.saturating_add(delta).clamp(0, len as isize - 1) as usize;
        if next == self.repl.selected() {
            return false;
        }
        self.repl.select(next);
        // A new frame is a new source extent and a new locals list; keeping the
        // old offsets would scroll the new pane to an unrelated position.
        self.source_scroll = 0;
        self.locals_scroll = 0;
        true
    }
}

/// Whether the interactive TUI can be used, i.e. whether there is a terminal to
/// take over.
///
/// Both halves matter and for different reasons. Without a stdout terminal there
/// is no screen to draw on. Without a stdin terminal there are no keystrokes to
/// read, so the event loop would spin against EOF and the user would see a frozen
/// screen — which is what `--debug=always` in a script, or a piped command list,
/// looks like from in here. Either way the caller should run the line REPL.
#[must_use]
pub fn should_use_tui() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Take over the terminal and run the debugger until the user quits, then give
/// the terminal back and return the [`Repl`] for teardown.
///
/// Setup and teardown go through `ratatui::try_init`/`restore`, which enter raw
/// mode and the alternate screen *and* install a panic hook that restores both
/// before the panic is printed. That hook is the part worth not hand-rolling:
/// raw mode outlives the process that set it, so a panic escaping with it still
/// on leaves the user at a shell that does not echo what they type.
///
/// Mouse capture is ours to add and remove — `try_init` does not enable it, and
/// leaving it on would make the terminal swallow selection and scrolling after
/// the debugger exits.
pub fn run(mut tui: Tui) -> std::io::Result<Repl> {
    let mut terminal = ratatui::try_init()?;
    let mouse = execute!(std::io::stdout(), EnableMouseCapture);

    let result = event_loop(&mut terminal, &mut tui);

    if mouse.is_ok() {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
    }
    ratatui::restore();
    let _ = std::io::stdout().flush();

    result.map(|()| tui.into_repl())
}

/// Draw, wait for an event, apply it; repeat until something sets `quit`.
///
/// Split out from [`run`] so that every early return still passes through that
/// function's restore path — a `?` in here must not skip putting the terminal
/// back.
fn event_loop(terminal: &mut ratatui::DefaultTerminal, tui: &mut Tui) -> std::io::Result<()> {
    while !tui.quit {
        terminal.draw(|frame| draw(frame, tui))?;
        match event::read()? {
            // Press and Repeat, never Release. A terminal with kitty-style key
            // reporting sends Press *and* Release for one keystroke, and acting on
            // both moves two frames per press. Repeat has to be accepted, though,
            // or holding an arrow down on such a terminal moves exactly one frame
            // and then appears to stop responding.
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                handle_key(tui, key)
            }
            Event::Mouse(m) => handle_mouse(tui, m),
            _ => {}
        }
    }
    Ok(())
}

/// Route a keypress by mode.
fn handle_key(tui: &mut Tui, key: KeyEvent) {
    match tui.mode {
        Mode::Help => {
            // Any key dismisses the overlay. Not a no-op for `q`: dismissing is
            // what the user meant, and quitting the debugger from a help screen
            // would be a surprise.
            tui.mode = Mode::Normal;
        }
        Mode::Command => handle_command_key(tui, key),
        Mode::Normal => handle_normal_key(tui, key),
    }
}

fn handle_normal_key(tui: &mut Tui, key: KeyEvent) {
    // Ctrl-C quits from anywhere. In raw mode no signal is delivered, so this is
    // the only thing that makes the conventional key work at all.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        tui.quit = true;
        return;
    }
    match key.code {
        KeyCode::Char('q') => tui.quit = true,
        KeyCode::Char('?') => tui.mode = Mode::Help,
        KeyCode::Char(':') => {
            tui.mode = Mode::Command;
            tui.input.clear();
            tui.input_cursor = 0;
            tui.history_pos = None;
        }
        KeyCode::Tab => tui.focus = tui.focus.next(),
        KeyCode::BackTab => tui.focus = tui.focus.prev(),
        // Frame navigation is global rather than only-when-the-backtrace-is-
        // focused: it is the debugger's primary motion, and requiring a Tab first
        // would make the common case the awkward one. Inside a scrollable pane
        // the same keys scroll that pane instead.
        //
        // These four keys are **spatial**: they move the highlight the way it
        // points on screen. The backtrace is drawn innermost-first, so `↑` means
        // a *lower* frame number. Wiring them to the call-stack sense instead —
        // `↑` for "toward the caller" — inverts them against the list the user is
        // looking at: `↑` on frame 0 jumps the marker downward, and `↓` there does
        // nothing at all, because frame 0 is already the top row. Which reads, and
        // was reported, as the arrows not working.
        //
        // The call-stack sense keeps its own keys below, where no screen direction
        // is implied to contradict.
        KeyCode::Up | KeyCode::Char('k') => match tui.focus {
            Focus::Source => tui.source_scroll = tui.source_scroll.saturating_sub(1),
            Focus::Locals => tui.locals_scroll = tui.locals_scroll.saturating_sub(1),
            Focus::Output => tui.output_scroll = tui.output_scroll.saturating_sub(1),
            Focus::Backtrace => {
                tui.move_frame(-1);
            }
        },
        KeyCode::Down | KeyCode::Char('j') => match tui.focus {
            Focus::Source => tui.source_scroll = tui.source_scroll.saturating_add(1),
            Focus::Locals => tui.locals_scroll = tui.locals_scroll.saturating_add(1),
            Focus::Output => tui.output_scroll = tui.output_scroll.saturating_add(1),
            Focus::Backtrace => {
                tui.move_frame(1);
            }
        },
        // `u`/`d` are the *call-stack* sense — the same direction the `up` and
        // `down` commands mean, so pressing `u` and typing `:up` cannot disagree.
        // `u` is the caller, which on an innermost-first list is downward on
        // screen; that is why it is not bound to an arrow. They work from any
        // pane, so the chain stays reachable without tabbing back.
        KeyCode::Char('u') => {
            tui.move_frame(1);
        }
        KeyCode::Char('d') => {
            tui.move_frame(-1);
        }
        // The ends of the list, spatially like the arrows: Home is the top row
        // (frame 0, the innermost), End the bottom. Expressed as a big move so the
        // clamping and the scroll reset stay in one place.
        KeyCode::Home => {
            tui.move_frame(isize::MIN);
        }
        KeyCode::End => {
            tui.move_frame(isize::MAX);
        }
        // A page moves whatever the focused pane's own unit is: frames in the
        // backtrace, lines elsewhere — and in the same direction as the arrows.
        KeyCode::PageUp if tui.focus == Focus::Backtrace => {
            tui.move_frame(-10);
        }
        KeyCode::PageDown if tui.focus == Focus::Backtrace => {
            tui.move_frame(10);
        }
        KeyCode::PageUp => bump_scroll(tui, -10),
        KeyCode::PageDown => bump_scroll(tui, 10),
        // The commands worth a single key. Each still goes through `handle`, so
        // the key is a shorthand for the command and not a parallel path.
        KeyCode::Char('r') => tui.run_command("restart"),
        KeyCode::Char('R') => tui.run_command("reload"),
        KeyCode::Char('l') => tui.run_command("locals"),
        KeyCode::Char('b') => tui.run_command("bt"),
        KeyCode::Char('i') => tui.run_command("input"),
        KeyCode::Char('P') => tui.run_command("parser"),
        // `p` is the evaluate command, so the key opens the command line already
        // primed with it rather than running something else that starts with a p.
        // Pressing `p` and typing an expression is the thing you actually want.
        KeyCode::Char('p') => {
            tui.mode = Mode::Command;
            tui.input = "p ".to_string();
            tui.input_cursor = tui.input.len();
            tui.history_pos = None;
        }
        _ => {}
    }
}

/// Scroll the focused pane by `delta` lines.
fn bump_scroll(tui: &mut Tui, delta: i32) {
    let apply = |v: u16| -> u16 {
        if delta < 0 {
            v.saturating_sub(delta.unsigned_abs() as u16)
        } else {
            v.saturating_add(delta as u16)
        }
    };
    match tui.focus {
        Focus::Source => tui.source_scroll = tui.source_scroll.saturating_add(delta),
        Focus::Locals => tui.locals_scroll = apply(tui.locals_scroll),
        // The backtrace scrolls itself to keep the selection visible, so a scroll
        // aimed at it belongs to the transcript instead. Frame *movement* is
        // handled before this is reached.
        Focus::Output | Focus::Backtrace => tui.output_scroll = apply(tui.output_scroll),
    }
}

fn handle_command_key(tui: &mut Tui, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            tui.mode = Mode::Normal;
            tui.input.clear();
            tui.input_cursor = 0;
        }
        KeyCode::Enter => {
            let line = std::mem::take(&mut tui.input);
            tui.input_cursor = 0;
            tui.mode = Mode::Normal;
            if !line.trim().is_empty() {
                // Don't record a command twice in a row; a repeated `locals` is
                // one history entry, not five.
                if tui.history.last().map(String::as_str) != Some(line.trim()) {
                    tui.history.push(line.trim().to_string());
                }
            }
            tui.history_pos = None;
            tui.run_command(&line);
        }
        KeyCode::Backspace => {
            // Delete the char *before* the cursor, which means finding its
            // boundary — `input_cursor - 1` is not a char boundary for anything
            // outside ASCII.
            if tui.input_cursor > 0 {
                let prev = tui.input[..tui.input_cursor]
                    .char_indices()
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                tui.input.replace_range(prev..tui.input_cursor, "");
                tui.input_cursor = prev;
            }
        }
        KeyCode::Delete => {
            if tui.input_cursor < tui.input.len() {
                let next = next_boundary(&tui.input, tui.input_cursor);
                tui.input.replace_range(tui.input_cursor..next, "");
            }
        }
        KeyCode::Left => {
            if tui.input_cursor > 0 {
                tui.input_cursor = tui.input[..tui.input_cursor]
                    .char_indices()
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
            }
        }
        KeyCode::Right => {
            if tui.input_cursor < tui.input.len() {
                tui.input_cursor = next_boundary(&tui.input, tui.input_cursor);
            }
        }
        KeyCode::Home => tui.input_cursor = 0,
        KeyCode::End => tui.input_cursor = tui.input.len(),
        // History recall. `history_pos` counts back from the newest entry.
        KeyCode::Up => {
            if !tui.history.is_empty() {
                let pos = match tui.history_pos {
                    None => tui.history.len() - 1,
                    Some(0) => 0,
                    Some(p) => p - 1,
                };
                tui.history_pos = Some(pos);
                tui.input = tui.history[pos].clone();
                tui.input_cursor = tui.input.len();
            }
        }
        KeyCode::Down => match tui.history_pos {
            Some(p) if p + 1 < tui.history.len() => {
                tui.history_pos = Some(p + 1);
                tui.input = tui.history[p + 1].clone();
                tui.input_cursor = tui.input.len();
            }
            Some(_) => {
                // Past the newest entry is the empty line the user was typing.
                tui.history_pos = None;
                tui.input.clear();
                tui.input_cursor = 0;
            }
            None => {}
        },
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                if c == 'c' {
                    tui.mode = Mode::Normal;
                    tui.input.clear();
                    tui.input_cursor = 0;
                }
                return;
            }
            tui.input.insert(tui.input_cursor, c);
            tui.input_cursor += c.len_utf8();
        }
        _ => {}
    }
}

/// The byte offset of the char boundary after `at`.
fn next_boundary(s: &str, at: usize) -> usize {
    s[at..]
        .chars()
        .next()
        .map(|c| at + c.len_utf8())
        .unwrap_or(at)
}

fn handle_mouse(tui: &mut Tui, m: event::MouseEvent) {
    use event::MouseEventKind;
    match m.kind {
        MouseEventKind::ScrollUp => bump_scroll(tui, -3),
        MouseEventKind::ScrollDown => bump_scroll(tui, 3),
        _ => {}
    }
}

// ---- rendering ----

/// Draw one frame: the fault banner, the four panes, and the status/command bar.
fn draw(frame: &mut Frame, tui: &mut Tui) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            // The banner: the fault, which is the one thing always worth a line.
            Constraint::Length(1),
            Constraint::Min(6),
            // The status bar / command line.
            Constraint::Length(1),
        ])
        .split(frame.area());

    draw_banner(frame, rows[0], tui);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        // The left column is a list of short rows (frames, `name: Type = value`);
        // the right holds source lines, which are as wide as the program's
        // indentation makes them. So the split favors the right.
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(rows[1]);

    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(cols[0]);

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
        .split(cols[1]);

    draw_backtrace(frame, left[0], tui);
    draw_locals(frame, left[1], tui);
    draw_source(frame, right[0], tui);
    draw_output(frame, right[1], tui);

    draw_status(frame, rows[2], tui);

    if tui.mode == Mode::Help {
        draw_help_overlay(frame);
    }
}

/// A pane border, brightened when the pane holds focus so the eye can find what
/// the keys are driving without reading the status bar.
fn pane_block(title: &str, focused: bool) -> Block<'_> {
    let (border, title_style) = if focused {
        (
            Style::default().fg(theme::FOCUS),
            Style::default()
                .fg(theme::FOCUS)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        (
            Style::default().fg(theme::CHROME),
            Style::default().fg(theme::CHROME),
        )
    };
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(Span::styled(format!(" {title} "), title_style))
}

fn draw_banner(frame: &mut Frame, area: Rect, tui: &Tui) {
    let mut spans = vec![
        Span::styled(
            " ✗ ",
            Style::default()
                .fg(theme::FAULT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            tui.fault.to_string(),
            Style::default()
                .fg(theme::FAULT)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    // For `panic`/`assert` the message *is* the diagnosis — the kind alone says
    // nothing the program did not already say (the same reasoning
    // `render_noninteractive` applies).
    if let Some(msg) = &tui.fault_message {
        spans.push(Span::styled(": ", Style::default().fg(theme::MUTED)));
        spans.push(Span::styled(msg.clone(), Style::default().fg(theme::FAULT)));
    }
    spans.push(Span::styled(
        format!("  ·  {} frame(s)", tui.repl.snapshot().len()),
        Style::default().fg(theme::MUTED),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_backtrace(frame: &mut Frame, area: Rect, tui: &Tui) {
    let snap = tui.repl.snapshot();
    let selected = tui.repl.selected();
    let source = tui.repl.session().map(|s| s.source_text.clone());
    // Align the `:line` column so the names read as a column and the numbers do
    // not wander in behind them.
    let name_width = (0..snap.len())
        // SAFETY: as below — compiler-embedded 'static UTF-8.
        .map(|i| unsafe { snap.frame_name(i) }.len())
        .max()
        .unwrap_or(0)
        .min(24);
    let mut lines: Vec<Line> = Vec::new();
    for i in 0..snap.len() {
        // SAFETY: names are compiler-embedded 'static UTF-8 (the same contract
        // `render_backtrace` relies on).
        let name = unsafe { snap.frame_name(i) };
        let is_sel = i == selected;
        // The marker replaces the REPL's trailing "(frame N selected)" line: the
        // selection belongs on the row it describes.
        let marker = if is_sel { "▶ " } else { "  " };
        let num_style = if is_sel {
            Style::default().fg(theme::FOCUS)
        } else {
            Style::default().fg(theme::MUTED)
        };
        let name_style = if is_sel {
            Style::default()
                .fg(theme::NAME)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let mut spans = vec![
            Span::styled(marker, Style::default().fg(theme::FOCUS)),
            Span::styled(format!("#{i:<2} "), num_style),
            Span::styled(format!("{name:<name_width$}"), name_style),
        ];
        // The frame's line number, when it resolves — a backtrace without
        // locations makes you open the source to place any frame but the top.
        if let (Some(src), Some(f)) = (source.as_deref(), snap.frames.get(i)) {
            if let Some(line_no) = span_line(src, f) {
                spans.push(Span::styled(
                    format!(" :{line_no}"),
                    Style::default().fg(theme::MUTED),
                ));
            }
        }
        lines.push(Line::from(spans));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no frames)",
            Style::default().fg(theme::MUTED),
        )));
    }
    // Keep the selected frame on screen for a chain deeper than the pane.
    let inner_height = area.height.saturating_sub(2) as usize;
    let scroll = if inner_height > 0 && selected >= inner_height {
        (selected - inner_height + 1) as u16
    } else {
        0
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(pane_block(
                Focus::Backtrace.title(),
                tui.focus == Focus::Backtrace,
            ))
            .scroll((scroll, 0)),
        area,
    );
}

fn draw_locals(frame: &mut Frame, area: Rect, tui: &Tui) {
    let snap = tui.repl.snapshot();
    let selected = tui.repl.selected();
    let (db, source) = match tui.repl.session() {
        Some(s) => (Some(&s.analysis.db), Some(s.source_text.as_str())),
        None => (None, None),
    };

    let mut lines: Vec<Line> = Vec::new();
    match snap.frames.get(selected) {
        None => lines.push(muted_line("  (no frame selected)")),
        Some(frame_ref) => {
            // The same split, and the same dead-scratch filter, as
            // `render_frame_locals`: a temp with neither a value nor a span holds
            // nothing and explains nothing.
            let mut users: Vec<&DebugLocal> = Vec::new();
            let mut temps: Vec<&DebugLocal> = Vec::new();
            for local in &frame_ref.locals {
                if local.is_user() {
                    users.push(local);
                } else if local.value.is_some() || local.span().is_some() {
                    temps.push(local);
                }
            }
            if users.is_empty() && temps.is_empty() {
                lines.push(muted_line("  (no locals in this frame)"));
            }
            // Align both the name and the type column across the whole frame, so
            // the values line up as a column instead of stepping in and out
            // behind types of different widths.
            let name_width = users
                .iter()
                .map(|l| l.name().len())
                .chain(temps.iter().map(|l| 4 + digits(l.symbol_id)))
                .max()
                .unwrap_or(0)
                .min(20);
            let type_width = users
                .iter()
                .chain(temps.iter())
                .map(|l| type_of(l, db).len())
                .max()
                .unwrap_or(0)
                .min(22);
            // Budget the value column to the width actually left in the pane.
            //
            // Without this the truncator cuts at an element boundary and then the
            // pane clips whatever still overhangs, mid-element, at its edge —
            // `[0, 1, 2, 3, 4, 5, 6, ` — which is the exact ragged ending the
            // boundary cut exists to avoid. The budget has to be the space, not a
            // constant that happens to be bigger than it.
            let widths = ColumnWidths::for_pane(area.width, name_width, type_width);
            if !users.is_empty() {
                lines.push(section_line("bindings"));
                for local in &users {
                    lines.push(local_line(local, db, source, &widths, false));
                }
            }
            if !temps.is_empty() {
                lines.push(section_line("temps"));
                for local in &temps {
                    lines.push(local_line(local, db, source, &widths, true));
                }
            }
        }
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(pane_block(
                Focus::Locals.title(),
                tui.focus == Focus::Locals,
            ))
            .scroll((tui.locals_scroll, 0)),
        area,
    );
}

/// The locals pane's column layout, fitted to the pane's actual width.
///
/// The name and type columns are as wide as their widest entry; the value gets
/// what is left, and a temp's provenance takes a slice of that when there is room
/// for both to be worth showing. `tmp#6` says nothing without its `xs[scaled]`, so
/// the provenance is not simply dropped when space is tight — it is given a share.
#[derive(Debug)]
struct ColumnWidths {
    name: usize,
    ty: usize,
    value: usize,
    provenance: usize,
}

impl ColumnWidths {
    fn for_pane(pane_width: u16, name: usize, ty: usize) -> ColumnWidths {
        // Mirror `local_line`'s own spans: 2 indent + name + 1 + [type + 1] + 2
        // for `= `.
        let fixed = 2 + name + 1 + if ty > 0 { ty + 1 } else { 0 } + 2;
        // 2 for the pane borders.
        let free = (pane_width as usize)
            .saturating_sub(2)
            .saturating_sub(fixed);
        // Below this there is no room to share; the value takes what exists and
        // the provenance waits for a wider pane.
        let (value, provenance) = if free >= 34 {
            let prov = (free / 3).max(12);
            // `- 1` for the column that separates the value from the provenance;
            // without it the pair is exactly one wider than the pane and the
            // provenance loses its last character to the border.
            (free - prov - 1, prov)
        } else {
            (free, 0)
        };
        ColumnWidths {
            name,
            ty,
            // A floor, so a very narrow pane shows a stub rather than nothing at
            // all — the row is then clipped by the pane, which is the honest
            // outcome when the terminal is too narrow for the data.
            value: value.max(8),
            provenance,
        }
    }
}

/// One locals row: `name  Type  = value`, colored by role.
fn local_line<'a>(
    local: &DebugLocal,
    db: Option<&praxis_types::TypeDb>,
    source: Option<&str>,
    widths: &ColumnWidths,
    is_temp: bool,
) -> Line<'a> {
    let ty = type_of(local, db);
    let value = match local.value {
        Some(v) => crate::value::format_bounded(v, widths.value),
        // The absence is the type's, not a sentinel's (F18): a slot nothing was
        // spilled into reads back as `None`.
        None => "<uninit>".to_string(),
    };
    let label = if is_temp {
        format!("tmp#{}", local.symbol_id)
    } else {
        let n = local.name();
        if n.is_empty() {
            // Unreachable from compiled code — `VerifyError::UserLocalHasNoName`
            // rejects a nameless binding — but hand-built frames still reach it.
            "?".to_string()
        } else {
            n.to_string()
        }
    };
    let label_style = if is_temp {
        Style::default().fg(theme::MUTED)
    } else {
        Style::default().fg(theme::NAME)
    };
    let value_style = if local.value.is_some() {
        Style::default().fg(theme::VALUE)
    } else {
        Style::default()
            .fg(theme::MUTED)
            .add_modifier(Modifier::ITALIC)
    };

    let name_width = widths.name;
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(format!("{label:<name_width$}"), label_style),
        Span::raw(" "),
    ];
    if widths.ty > 0 {
        let type_width = widths.ty;
        spans.push(Span::styled(
            format!("{ty:<type_width$}"),
            Style::default().fg(theme::TYPE),
        ));
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled("= ", Style::default().fg(theme::MUTED)));
    // Pad the value out to its column so a temp's provenance starts at the same
    // place on every row instead of hugging whatever the value happened to be.
    let pad = widths.value.saturating_sub(value.chars().count());
    spans.push(Span::styled(value, value_style));
    // A temp's materializing expression is the only thing that says which part of
    // the line it belongs to, so it earns its place on the row.
    if is_temp && widths.provenance > 0 {
        if let Some(expr) = provenance_of(local, source, widths.provenance) {
            spans.push(Span::raw(" ".repeat(pad + 1)));
            spans.push(Span::styled(
                expr,
                Style::default()
                    .fg(theme::MUTED)
                    .add_modifier(Modifier::ITALIC),
            ));
        }
    }
    Line::from(spans)
}

fn draw_source(frame: &mut Frame, area: Rect, tui: &mut Tui) {
    let snap = tui.repl.snapshot();
    let selected = tui.repl.selected();
    // SAFETY: names are compiler-embedded 'static UTF-8.
    let fn_name = if selected < snap.len() {
        unsafe { snap.frame_name(selected) }.to_string()
    } else {
        "source".to_string()
    };
    let span = snap
        .frames
        .get(selected)
        .map(|f| f.source_span)
        .unwrap_or((0, 0));
    let (source, path) = match tui.repl.session() {
        Some(s) => (
            Some(s.source_text.clone()),
            s.source_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
        ),
        None => (None, String::new()),
    };

    let title = if path.is_empty() {
        fn_name.clone()
    } else {
        format!("{fn_name} · {path}")
    };

    let mut lines: Vec<Line> = Vec::new();
    // Which rendered row the fault marker landed on, so the pane can scroll to it
    // rather than to the top of the function.
    let mut fault_row: Option<usize> = None;
    match source.as_deref() {
        None => lines.push(muted_line("  (no source — session not attached)")),
        Some(src) => {
            if span == (0, 0) {
                lines.push(muted_line("  (no source span recorded for this frame)"));
            } else {
                let (start, end) = (span.0 as usize, span.1 as usize);
                if start >= src.len() || end > src.len() || start > end {
                    lines.push(muted_line("  (source span is outside the program source)"));
                } else {
                    // Show the frame's whole extent, and mark the line the fault
                    // is on — for frame 0 the faulting line, for a caller the
                    // call that led there. The frame's own span starts at `fn`,
                    // so the marked line comes from `fault_span`, not from here.
                    let first = line_index(src, start);
                    let last = line_index(src, end.saturating_sub(1));
                    let fault = snap.frames.get(selected).and_then(fault_span);
                    let fault_line = fault.map(|(s, _)| line_index(src, s as usize));
                    let width = digits((last + 1) as u32);
                    for (n, text) in src.lines().enumerate().take(last + 1).skip(first) {
                        let is_fault = Some(n) == fault_line;
                        if is_fault {
                            fault_row = Some(lines.len());
                        }
                        let marker = if is_fault { "▶" } else { " " };
                        let mut spans = vec![
                            Span::styled(
                                marker,
                                Style::default()
                                    .fg(theme::FAULT)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(
                                format!(" {:>width$} ", n + 1, width = width),
                                Style::default().fg(theme::MUTED),
                            ),
                            Span::styled("│ ", Style::default().fg(theme::CHROME)),
                        ];
                        // On the faulting line, pick the faulting expression out
                        // of the rest of it. A whole bold line says "somewhere
                        // here"; the columns say which subexpression.
                        let cols = if is_fault {
                            fault.and_then(|sp| span_cols(src, n, text, sp))
                        } else {
                            None
                        };
                        match cols {
                            Some((a, b)) => {
                                spans.push(Span::raw(text[..a].to_string()));
                                spans.push(Span::styled(
                                    text[a..b].to_string(),
                                    Style::default()
                                        .fg(theme::FAULT)
                                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                                ));
                                spans.push(Span::raw(text[b..].to_string()));
                            }
                            None => spans.push(Span::styled(
                                text.to_string(),
                                if is_fault {
                                    Style::default().add_modifier(Modifier::BOLD)
                                } else {
                                    Style::default()
                                },
                            )),
                        }
                        lines.push(Line::from(spans));
                    }
                }
            }
        }
    }
    let scroll = source_anchor(
        lines.len(),
        fault_row,
        area.height.saturating_sub(2) as usize,
        tui.source_scroll,
    );
    frame.render_widget(
        Paragraph::new(lines)
            .block(pane_block(&title, tui.focus == Focus::Source))
            .scroll((scroll, 0)),
        area,
    );
}

fn draw_output(frame: &mut Frame, area: Rect, tui: &Tui) {
    let lines: Vec<Line> = tui
        .output
        .iter()
        .map(|l| {
            // The echoed command, the errors, and everything else read
            // differently and should look different.
            if let Some(cmd) = l.strip_prefix("❯ ") {
                Line::from(vec![
                    Span::styled("❯ ", Style::default().fg(theme::FOCUS)),
                    Span::styled(
                        cmd.to_string(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                ])
            } else if l.starts_with("error:") || l.starts_with("unknown command") {
                Line::from(Span::styled(l.clone(), Style::default().fg(theme::FAULT)))
            } else {
                Line::from(Span::raw(l.clone()))
            }
        })
        .collect();
    // Clamp the scroll so the transcript cannot be scrolled past its end into
    // blank space.
    let inner = area.height.saturating_sub(2);
    let max_scroll = (lines.len() as u16).saturating_sub(inner);
    frame.render_widget(
        Paragraph::new(lines)
            .block(pane_block(
                Focus::Output.title(),
                tui.focus == Focus::Output,
            ))
            .wrap(Wrap { trim: false })
            .scroll((tui.output_scroll.min(max_scroll), 0)),
        area,
    );
}

fn draw_status(frame: &mut Frame, area: Rect, tui: &Tui) {
    if tui.mode == Mode::Command {
        // The command line, with a visible block cursor: a prompt you cannot see
        // the caret in feels unresponsive.
        let (before, after) = tui.input.split_at(tui.input_cursor);
        let (cursor_char, rest) = match after.chars().next() {
            Some(c) => (c.to_string(), &after[c.len_utf8()..]),
            None => (" ".to_string(), ""),
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    " ❯ ",
                    Style::default()
                        .fg(theme::FOCUS)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(before.to_string()),
                Span::styled(
                    cursor_char,
                    Style::default().add_modifier(Modifier::REVERSED),
                ),
                Span::raw(rest.to_string()),
            ])),
            area,
        );
        return;
    }
    let mut spans = vec![Span::styled(
        format!(" {} ", tui.focus.title()),
        Style::default()
            .bg(theme::FOCUS)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD),
    )];
    for (key, label) in [
        ("↑↓", "frame"),
        ("tab", "pane"),
        (":", "cmd"),
        ("r", "restart"),
        ("?", "keys"),
        ("q", "quit"),
    ] {
        spans.push(Span::styled(
            format!("  {key}"),
            Style::default().fg(theme::KEY).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {label}"),
            Style::default().fg(theme::MUTED),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The `?` overlay: the key bindings, plus the commands the `:` line accepts.
/// Reads no state — the bindings are the same on every frame.
fn draw_help_overlay(frame: &mut Frame) {
    let area = centered(frame.area(), 62, 24);
    frame.render_widget(Clear, area);

    let mut lines = vec![Line::from(Span::styled(
        "navigation",
        Style::default()
            .fg(theme::FOCUS)
            .add_modifier(Modifier::BOLD),
    ))];
    for (key, what) in [
        ("↑ / k", "select the frame above (toward #0)"),
        ("↓ / j", "select the frame below"),
        ("home / end", "first / last frame in the list"),
        ("u / d", "up / down the call stack, from any pane"),
        ("tab", "move focus between panes"),
        ("pgup / pgdn", "a page of the focused pane"),
    ] {
        lines.push(help_row(key, what));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "shortcuts",
        Style::default()
            .fg(theme::FOCUS)
            .add_modifier(Modifier::BOLD),
    )));
    for (key, what) in [
        ("p", "start `p ` — evaluate an expression"),
        ("r / R", "restart / reload"),
        ("l / b", "locals / backtrace into output"),
        ("i / P", "input / parser context"),
        (": …", "run any command"),
        ("q", "quit the debugger"),
    ] {
        lines.push(help_row(key, what));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "commands (`:`)",
        Style::default()
            .fg(theme::FOCUS)
            .add_modifier(Modifier::BOLD),
    )));
    for (key, what) in [
        ("p EXPR", "evaluate a read-only expression"),
        ("type EXPR", "show the inferred type"),
        ("heap EXPR", "inspect a value with its type"),
        ("frame N", "select frame N"),
        ("source [N]", "print a frame's source"),
        ("restart / reload", "rerun / recompile and rerun"),
    ] {
        lines.push(help_row(key, what));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  any key to dismiss",
        Style::default()
            .fg(theme::MUTED)
            .add_modifier(Modifier::ITALIC),
    )));

    frame.render_widget(Paragraph::new(lines).block(pane_block("keys", true)), area);
}

fn help_row<'a>(key: &'a str, what: &'a str) -> Line<'a> {
    Line::from(vec![
        Span::styled(
            format!("  {key:<18}"),
            Style::default().fg(theme::KEY).add_modifier(Modifier::BOLD),
        ),
        Span::styled(what, Style::default().fg(theme::MUTED)),
    ])
}

/// A `w`×`h` rect centered in `area`, clamped to it so a small terminal still
/// gets a usable overlay instead of one drawn off-screen.
fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn muted_line<'a>(text: &str) -> Line<'a> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default().fg(theme::MUTED),
    ))
}

fn section_line<'a>(text: &str) -> Line<'a> {
    Line::from(Span::styled(
        format!(" {text}"),
        Style::default()
            .fg(theme::CHROME)
            .add_modifier(Modifier::BOLD),
    ))
}

/// The local's static type, rendered from the live `TypeDb`. Empty when there is
/// no db, when the descriptor is null (a frame built before type threading), or
/// when the id is not one this db minted (F5) — the caller omits the column.
fn type_of(local: &DebugLocal, db: Option<&praxis_types::TypeDb>) -> String {
    let Some(db) = db else {
        return String::new();
    };
    if local.descriptor.is_null() {
        return String::new();
    }
    match db.type_from_raw(local.type_id) {
        Some(ty) => db.render(ty),
        None => String::new(),
    }
}

/// The temp's materializing expression, whitespace-collapsed to one line and cut
/// to `budget` so it cannot push the row past the pane.
fn provenance_of(local: &DebugLocal, source: Option<&str>, budget: usize) -> Option<String> {
    let source = source?;
    let (start, end) = local.span()?;
    let s = usize::try_from(start).ok()?;
    let e = usize::try_from(end).ok()?;
    if s >= source.len() || e > source.len() || s > e {
        return None;
    }
    let collapsed: String = source[s..e]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if collapsed.is_empty() {
        None
    } else {
        // `elide`, not the value truncator: this is source text, and balancing its
        // brackets would turn a cut inside `xs[scaled]` into `xs[sc…]` — an index
        // expression the program never wrote.
        Some(crate::value::elide(&collapsed, budget))
    }
}

/// The 0-based index of the line containing byte `offset`.
fn line_index(src: &str, offset: usize) -> usize {
    let capped = offset.min(src.len());
    src[..capped].bytes().filter(|b| *b == b'\n').count()
}

/// The 1-based line number to show for a frame in the backtrace: where the fault
/// is, falling back to where the function starts.
fn span_line(src: &str, frame: &praxis_runtime::crash_snapshot::SnapshotFrame) -> Option<usize> {
    let span = fault_span(frame).unwrap_or(frame.source_span);
    if span == (0, 0) {
        return None;
    }
    let start = span.0 as usize;
    if start >= src.len() {
        return None;
    }
    Some(line_index(src, start) + 1)
}

/// The span of the expression this frame faulted in, if it can be recovered.
///
/// A frame's own `source_span` covers the whole function, so it cannot answer
/// "which line?" — pointing at `fn pick(…) {` is pointing at the wrong line in
/// every function longer than one. What can answer it is the temps: a compiler
/// temp that carries a source span but never received a value is an expression
/// that started evaluating and did not finish. In the frame that faulted that is
/// the faulting expression; in a caller it is the call that led there, which is
/// exactly the line that frame should be showing.
///
/// The *narrowest* such span is the innermost such expression — `xs[scaled]`
/// rather than the `return xs[scaled]` that encloses it — which is the one worth
/// pointing at.
///
/// This is the same signal `render_frame_locals` already trusts when it keeps an
/// uninit temp that carries a span: "a temp for an expression whose value
/// genuinely never computed … which is exactly what the user needs to see".
fn fault_span(frame: &praxis_runtime::crash_snapshot::SnapshotFrame) -> Option<(u32, u32)> {
    frame
        .locals
        .iter()
        .filter(|l| !l.is_user() && l.value.is_none())
        .filter_map(|l| l.span())
        .filter(|(s, e)| e > s)
        .min_by_key(|(s, e)| e - s)
}

/// The source pane's absolute scroll offset: where to start drawing so the
/// faulting row is on screen, plus whatever the user has scrolled from there.
///
/// A frame's extent is a whole function, so starting at row 0 hides the marked
/// line in any function taller than the pane — the fault would be found by
/// scrolling to it, on every frame change. Anchoring on the fault instead makes
/// `source_scroll == 0` mean "showing the fault", which is the useful default.
///
/// The anchor keeps the top of the function visible while the fault still fits on
/// screen — the signature is context worth having — and only once it does not fit
/// does it centre the fault line. The result is clamped so the pane never scrolls
/// past either end of the document.
fn source_anchor(total: usize, fault_row: Option<usize>, height: usize, delta: i32) -> u16 {
    let max_scroll = total.saturating_sub(height);
    let anchor = match fault_row {
        // Already visible from the top: leave the function's opening on screen.
        Some(row) if height > 0 && row >= height => row.saturating_sub(height / 2),
        _ => 0,
    };
    let resolved = (anchor as i64 + delta as i64).clamp(0, max_scroll as i64);
    u16::try_from(resolved).unwrap_or(u16::MAX)
}

/// The byte offset in `src` where 0-based `line` starts.
fn line_start(src: &str, line: usize) -> usize {
    let mut off = 0usize;
    for (i, l) in src.split('\n').enumerate() {
        if i == line {
            return off;
        }
        off += l.len() + 1;
    }
    off.min(src.len())
}

/// Where `span` falls within one line, as byte offsets into `text` (that line's
/// own bytes) — so the faulting subexpression can be styled apart from the rest
/// of the line it sits on.
///
/// `None` when the span does not lie inside this line at all, or when clamping it
/// to the line would not land on char boundaries: a highlight is a nicety, and
/// slicing a string at a non-boundary to get one would panic.
fn span_cols(src: &str, line: usize, text: &str, span: (u32, u32)) -> Option<(usize, usize)> {
    let ls = line_start(src, line);
    let (s, e) = (span.0 as usize, span.1 as usize);
    let a = s.checked_sub(ls)?;
    let b = e.checked_sub(ls)?.min(text.len());
    if a >= b || a >= text.len() {
        return None;
    }
    if !text.is_char_boundary(a) || !text.is_char_boundary(b) {
        return None;
    }
    Some((a, b))
}

/// Digits in `n`, for right-aligning the line-number gutter.
fn digits(n: u32) -> usize {
    if n == 0 {
        1
    } else {
        (n.ilog10() + 1) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use praxis_runtime::{crash_snapshot::SnapshotFrame, CrashSnapshot, FaultKind};

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

    fn tui() -> Tui {
        Tui::new(
            Repl::new(two_frame_snapshot()),
            FaultKind::IndexOutOfBounds,
            None,
        )
    }

    fn press(t: &mut Tui, code: KeyCode) {
        handle_key(t, KeyEvent::new(code, KeyModifiers::NONE));
    }

    /// The arrows are spatial: the backtrace is drawn innermost-first, so `↓`
    /// moves *down the list* to a higher frame number and `↑` back toward #0.
    ///
    /// Binding them the other way — `↑` for "toward the caller" — is what made
    /// them look broken: every session opens on frame 0, the top row, where `↑`
    /// jumped the marker downward and `↓` could do nothing at all.
    #[test]
    fn the_arrows_move_the_selection_the_way_they_point() {
        let mut t = tui();
        assert_eq!(t.repl.selected(), 0, "sessions open on the top row");
        // ↑ at the top row has nowhere to go, rather than jumping downward.
        press(&mut t, KeyCode::Up);
        assert_eq!(t.repl.selected(), 0, "↑ at the top row stays put");
        press(&mut t, KeyCode::Down);
        assert_eq!(t.repl.selected(), 1, "↓ moves down the list");
        press(&mut t, KeyCode::Up);
        assert_eq!(t.repl.selected(), 0, "↑ moves back up the list");
        // j/k are the same motion as the arrows, not the opposite of them.
        press(&mut t, KeyCode::Char('j'));
        assert_eq!(t.repl.selected(), 1, "j is ↓");
        press(&mut t, KeyCode::Char('k'));
        assert_eq!(t.repl.selected(), 0, "k is ↑");
    }

    /// Both ends clamp rather than wrap: wrapping from frame 0 to the outermost
    /// frame would silently move you across the whole program.
    #[test]
    fn frame_navigation_clamps_at_both_ends() {
        let mut t = tui();
        press(&mut t, KeyCode::Up);
        assert_eq!(t.repl.selected(), 0, "clamped at the top of the list");
        press(&mut t, KeyCode::Down);
        press(&mut t, KeyCode::Down);
        assert_eq!(t.repl.selected(), 1, "clamped at the bottom of the list");
    }

    /// `u`/`d` keep the *call-stack* sense, matching what the `up` and `down`
    /// commands do — so a keypress and the typed command cannot disagree. On an
    /// innermost-first list that makes `u` the opposite screen direction from `↑`,
    /// which is exactly why it is not bound to an arrow.
    #[test]
    fn u_and_d_follow_the_up_command_not_the_arrow() {
        let mut t = tui();
        press(&mut t, KeyCode::Char('u'));
        assert_eq!(t.repl.selected(), 1, "`u` selects the caller, like `:up`");
        press(&mut t, KeyCode::Char('d'));
        assert_eq!(t.repl.selected(), 0, "`d` selects the callee, like `:down`");
        // The `up` command agrees with the `u` key.
        let mut out = Vec::new();
        t.repl.handle("up", &mut out);
        assert_eq!(t.repl.selected(), 1, "the command moves the same way");
    }

    /// A held arrow arrives as `Repeat` on terminals with kitty-style key
    /// reporting. Dropping those made a held key move one frame and then look
    /// stuck; `Release` must still be ignored or one press moves two frames.
    #[test]
    fn repeat_events_navigate_and_release_events_do_not() {
        let mut t = tui();
        handle_key(&mut t, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(t.repl.selected(), 1);
        // The event loop's filter is what decides this, so assert on it directly.
        for kind in [KeyEventKind::Press, KeyEventKind::Repeat] {
            assert!(
                matches!(kind, KeyEventKind::Press | KeyEventKind::Repeat),
                "{kind:?} drives navigation"
            );
        }
        assert!(
            !matches!(
                KeyEventKind::Release,
                KeyEventKind::Press | KeyEventKind::Repeat
            ),
            "Release must not, or one keystroke moves twice"
        );
    }

    /// Changing frame resets the source/locals scroll: the offsets belonged to
    /// the frame you left, and carrying them over scrolls the new pane to an
    /// unrelated place.
    #[test]
    fn changing_frame_resets_pane_scroll() {
        let mut t = tui();
        t.focus = Focus::Source;
        press(&mut t, KeyCode::Down); // scrolls source, does not move frame
        assert_eq!(t.source_scroll, 1);
        press(&mut t, KeyCode::Char('u')); // frame move from any pane
        assert_eq!(t.repl.selected(), 1);
        assert_eq!(t.source_scroll, 0, "scroll reset with the frame");
    }

    /// `:` enters command mode; Esc leaves it without running anything.
    #[test]
    fn escape_abandons_a_typed_command() {
        let mut t = tui();
        press(&mut t, KeyCode::Char(':'));
        assert_eq!(t.mode, Mode::Command);
        for c in "frame 1".chars() {
            press(&mut t, KeyCode::Char(c));
        }
        assert_eq!(t.input, "frame 1");
        press(&mut t, KeyCode::Esc);
        assert_eq!(t.mode, Mode::Normal);
        assert!(t.input.is_empty(), "the abandoned line is cleared");
        assert_eq!(t.repl.selected(), 0, "the command never ran");
    }

    /// A command typed on the `:` line runs through `Repl::handle`, so the TUI
    /// and the line REPL cannot answer it differently — and its output lands in
    /// the transcript.
    #[test]
    fn a_command_runs_through_the_repl_and_is_echoed() {
        let mut t = tui();
        press(&mut t, KeyCode::Char(':'));
        for c in "frame 1".chars() {
            press(&mut t, KeyCode::Char(c));
        }
        press(&mut t, KeyCode::Enter);
        assert_eq!(t.mode, Mode::Normal);
        assert_eq!(t.repl.selected(), 1, "the command moved the frame cursor");
        assert!(
            t.output.iter().any(|l| l == "❯ frame 1"),
            "the command is echoed: {:?}",
            t.output
        );
        assert!(
            t.output.iter().any(|l| l.contains("main")),
            "its output is captured: {:?}",
            t.output
        );
    }

    /// `quit` typed as a command ends the loop, the same as `q`.
    #[test]
    fn the_quit_command_ends_the_loop() {
        let mut t = tui();
        t.run_command("quit");
        assert!(t.quit);
        let mut t2 = tui();
        press(&mut t2, KeyCode::Char('q'));
        assert!(t2.quit);
    }

    /// Ctrl-C quits. In raw mode no SIGINT is delivered, so without this the
    /// conventional key does nothing and the user is stuck in a full-screen app.
    #[test]
    fn ctrl_c_quits() {
        let mut t = tui();
        handle_key(
            &mut t,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );
        assert!(t.quit);
    }

    /// The help overlay is dismissed by any key — including `q`, which must not
    /// fall through and quit the debugger.
    #[test]
    fn help_is_dismissed_without_quitting() {
        let mut t = tui();
        press(&mut t, KeyCode::Char('?'));
        assert_eq!(t.mode, Mode::Help);
        press(&mut t, KeyCode::Char('q'));
        assert_eq!(t.mode, Mode::Normal, "the overlay was dismissed");
        assert!(!t.quit, "`q` on the help screen does not quit");
    }

    /// Command history recall, and the empty line past the newest entry.
    #[test]
    fn history_recalls_previous_commands() {
        let mut t = tui();
        t.run_command("bt");
        t.history.push("bt".to_string());
        t.history.push("locals".to_string());
        press(&mut t, KeyCode::Char(':'));
        press(&mut t, KeyCode::Up);
        assert_eq!(t.input, "locals", "↑ recalls the newest entry");
        press(&mut t, KeyCode::Up);
        assert_eq!(t.input, "bt", "↑ again walks back");
        press(&mut t, KeyCode::Down);
        assert_eq!(t.input, "locals");
        press(&mut t, KeyCode::Down);
        assert!(t.input.is_empty(), "past the newest entry is a fresh line");
    }

    /// Editing the command line has to move by characters, not bytes: a
    /// backspace that subtracts 1 from the cursor panics on a multi-byte char.
    #[test]
    fn command_line_editing_is_char_aware() {
        let mut t = tui();
        press(&mut t, KeyCode::Char(':'));
        for c in "p é".chars() {
            press(&mut t, KeyCode::Char(c));
        }
        assert_eq!(t.input, "p é");
        assert_eq!(t.input_cursor, t.input.len());
        press(&mut t, KeyCode::Backspace);
        assert_eq!(t.input, "p ", "the whole char was deleted");
        press(&mut t, KeyCode::Left);
        assert_eq!(t.input_cursor, 1);
        press(&mut t, KeyCode::Char('x'));
        assert_eq!(t.input, "px ");
    }

    /// `p` primes the command line with the evaluate command rather than running
    /// some other command that merely starts with a p.
    #[test]
    fn p_opens_the_command_line_ready_to_evaluate() {
        let mut t = tui();
        press(&mut t, KeyCode::Char('p'));
        assert_eq!(t.mode, Mode::Command);
        assert_eq!(t.input, "p ");
        assert_eq!(t.input_cursor, 2, "the cursor is past the prefix");
        // Typing continues the expression rather than replacing the prefix.
        for c in "1+1".chars() {
            press(&mut t, KeyCode::Char(c));
        }
        assert_eq!(t.input, "p 1+1");
    }

    /// A page in the backtrace moves frames, not the transcript — and in the same
    /// direction as the arrows.
    #[test]
    fn page_keys_move_frames_when_the_backtrace_has_focus() {
        let mut t = tui();
        assert_eq!(t.focus, Focus::Backtrace);
        press(&mut t, KeyCode::PageDown);
        assert_eq!(t.repl.selected(), 1, "clamped to the 2-frame chain");
        assert_eq!(t.output_scroll, 0, "the transcript did not move");
        press(&mut t, KeyCode::PageUp);
        assert_eq!(t.repl.selected(), 0);
        // In a scrollable pane the same key scrolls that pane instead.
        t.focus = Focus::Source;
        press(&mut t, KeyCode::PageDown);
        assert_eq!(t.source_scroll, 10);
    }

    /// Tab cycles focus in both directions and returns to where it started.
    #[test]
    fn tab_cycles_focus() {
        let mut t = tui();
        let start = t.focus;
        for _ in 0..4 {
            press(&mut t, KeyCode::Tab);
        }
        assert_eq!(t.focus, start, "four tabs is a full cycle");
        press(&mut t, KeyCode::BackTab);
        assert_eq!(t.focus, start.prev());
    }

    /// Home/End are spatial like the arrows: the first and last rows of the list.
    #[test]
    fn home_and_end_jump_to_the_ends_of_the_list() {
        let mut t = tui();
        press(&mut t, KeyCode::End);
        assert_eq!(t.repl.selected(), 1, "end is the last row");
        press(&mut t, KeyCode::Home);
        assert_eq!(t.repl.selected(), 0, "home is the first row");
    }

    /// A `DebugLocal` for the fault-span tests: a temp with a span and no value
    /// is what marks an expression that began evaluating and never finished.
    fn temp_with_span(symbol_id: u32, span: (u32, u32)) -> DebugLocal {
        DebugLocal {
            source_name: "".as_ptr(),
            name_len: 0,
            symbol_id,
            descriptor: std::ptr::NonNull::dangling().as_ptr(),
            value: None,
            type_id: 0,
            kind: praxis_runtime::LOCAL_KIND_TEMP,
            span_start: span.0,
            span_end: span.1,
        }
    }

    fn frame_with(locals: Vec<DebugLocal>, source_span: (u32, u32)) -> SnapshotFrame {
        let name: &'static str = "f";
        SnapshotFrame {
            parent: usize::MAX,
            func_name: name.as_ptr(),
            func_name_len: name.len() as u32,
            locals,
            source_span,
        }
    }

    /// The point of `fault_span`: the frame's own span starts at `fn`, so it
    /// cannot say which line faulted. The narrowest uninit temp span can, and it
    /// is the innermost expression rather than the `return` wrapping it.
    #[test]
    fn the_fault_span_is_the_narrowest_unfinished_expression() {
        // "return xs[scaled]" at 0..17, with "xs[scaled]" at 7..17 inside it.
        let frame = frame_with(
            vec![temp_with_span(1, (0, 17)), temp_with_span(2, (7, 17))],
            (0, 40),
        );
        assert_eq!(
            fault_span(&frame),
            Some((7, 17)),
            "the innermost unfinished expression, not the statement around it"
        );
    }

    /// A temp that *did* get a value finished evaluating, so it is not where the
    /// fault is — using it would point at the last expression that succeeded.
    #[test]
    fn a_temp_that_holds_a_value_is_not_the_fault_span() {
        let mut done = temp_with_span(1, (7, 9));
        done.value = Some(praxis_runtime::DebugValue::Scalar(
            praxis_runtime::ScalarValue::Int(3),
        ));
        let unfinished = temp_with_span(2, (0, 17));
        let frame = frame_with(vec![done, unfinished], (0, 40));
        assert_eq!(fault_span(&frame), Some((0, 17)));
    }

    /// No temps, or only empty spans, means no recoverable fault line — the
    /// backtrace then falls back to the function's own start.
    #[test]
    fn a_frame_without_unfinished_temps_has_no_fault_span() {
        assert_eq!(fault_span(&frame_with(Vec::new(), (5, 40))), None);
        // A zero-width span is not a location.
        assert_eq!(
            fault_span(&frame_with(vec![temp_with_span(1, (9, 9))], (5, 40))),
            None
        );
        let src = "fn f() {\n  x\n}";
        assert_eq!(
            span_line(src, &frame_with(Vec::new(), (11, 12))),
            Some(2),
            "falls back to the frame's own span"
        );
    }

    /// The backtrace reports the faulting line, not the line the function is
    /// declared on — that was the whole reason `fault_span` exists.
    #[test]
    fn the_backtrace_line_is_the_faulting_line() {
        let src = "fn f() {\n  var a = 1\n  return xs[a]\n}\n";
        // "xs[a]" sits on line 3, at bytes 30..35; the function starts at 0.
        let frame = frame_with(vec![temp_with_span(1, (30, 35))], (0, src.len() as u32));
        assert_eq!(span_line(src, &frame), Some(3), "line 3, not line 1");
    }

    /// The highlight columns are relative to the line, so the faulting
    /// subexpression can be styled apart from the rest of it.
    #[test]
    fn span_cols_locates_the_expression_within_its_line() {
        let src = "fn f() {\n  return xs[a]\n}\n";
        let line = 1;
        let text = "  return xs[a]";
        // "xs[a]" is at bytes 18..23 in `src`; line 1 starts at byte 9.
        assert_eq!(span_cols(src, line, text, (18, 23)), Some((9, 14)));
        assert_eq!(&text[9..14], "xs[a]", "the columns select the expression");
        // A span on a different line does not highlight this one.
        assert_eq!(span_cols(src, line, text, (0, 2)), None);
        // A span running past the line end clamps to it rather than panicking.
        assert!(span_cols(src, line, text, (18, 999)).is_some());
    }

    /// A multi-byte char before the span must not desynchronize the columns, and
    /// a cut landing inside one must decline rather than panic.
    #[test]
    fn span_cols_respects_char_boundaries() {
        // Bytes:      x0 ' '1 =2 ' '3 "4 é5-6 "7 ' '8 +9 ' '10 b11 a12 d13
        let src = "x = \"é\" + bad";
        let text = src;
        // Byte 6 is the middle of the two-byte 'é' — declining beats panicking in
        // the slice below.
        assert_eq!(span_cols(src, 0, text, (6, 8)), None);
        // A span on real boundaries resolves, and selects what it names.
        assert_eq!(span_cols(src, 0, text, (11, 14)), Some((11, 14)));
        assert_eq!(&text[11..14], "bad");
        // 'é' starts at byte 5, which *is* a boundary.
        assert_eq!(span_cols(src, 0, text, (5, 7)), Some((5, 7)));
    }

    /// The value budget must come from the pane, not from a constant: a constant
    /// larger than the space is what let the pane clip a boundary-cut value
    /// mid-element at its own edge.
    #[test]
    fn the_value_column_is_budgeted_to_the_pane() {
        // A 44-wide pane, `scaled` (6) names and `Vec[Int]` (8) types:
        // 2 borders + 2 indent + 6 + 1 + 8 + 1 + 2 = 22 fixed, 22 left.
        let w = ColumnWidths::for_pane(44, 6, 8);
        assert_eq!(w.value, 22, "the value gets exactly what the pane leaves");
        assert_eq!(w.provenance, 0, "no room to also show provenance here");
        // The value plus every fixed column must fit inside the pane, or the
        // truncation is decorative and the pane does the real cutting.
        assert!(2 + 2 + 6 + 1 + 8 + 1 + 2 + w.value <= 44);
    }

    /// Given room, a temp's provenance gets a share rather than being dropped —
    /// `tmp#6` is meaningless without the `xs[scaled]` beside it.
    #[test]
    fn a_wide_pane_shares_space_with_provenance() {
        let w = ColumnWidths::for_pane(120, 6, 8);
        assert!(
            w.provenance >= 12,
            "provenance gets a real share: {w:?}",
            w = w
        );
        assert!(
            w.value > w.provenance,
            "the value still gets the larger share"
        );
        assert!(2 + 2 + 6 + 1 + 8 + 1 + 2 + w.value + 1 + w.provenance <= 120);
    }

    /// A pane too narrow for the data yields a stub rather than a zero-width
    /// column, and never a panic.
    #[test]
    fn a_narrow_pane_still_yields_a_usable_column() {
        for width in 0u16..30 {
            let w = ColumnWidths::for_pane(width, 6, 8);
            assert!(
                w.value >= 8,
                "width {width} gave a value column of {}",
                w.value
            );
            assert_eq!(w.provenance, 0);
        }
    }

    /// A frame's extent is a whole function, so a fault deep inside a long one
    /// sits below the fold when the pane starts at row 0 — you had to scroll to
    /// find the marker on every frame change. The anchor is what fixes that.
    #[test]
    fn the_source_pane_scrolls_to_the_faulting_row() {
        // A 60-line function in a 20-row pane, faulting at row 45.
        let scroll = source_anchor(60, Some(45), 20, 0);
        assert!(
            (scroll as usize..scroll as usize + 20).contains(&45),
            "the faulting row is on screen, got scroll {scroll}"
        );
        // Centred rather than jammed against an edge, so there is context both ways.
        assert_eq!(scroll, 35);
    }

    /// While the fault still fits on the first screenful, stay at the top: the
    /// signature is worth seeing, and scrolling to centre a line that is already
    /// visible only throws context away.
    #[test]
    fn a_fault_already_on_screen_does_not_scroll() {
        assert_eq!(source_anchor(60, Some(5), 20, 0), 0);
        assert_eq!(
            source_anchor(60, Some(19), 20, 0),
            0,
            "the last visible row"
        );
        assert_eq!(
            source_anchor(60, Some(20), 20, 0),
            10,
            "one past it centres"
        );
    }

    /// The user's scrolling is a delta from the anchor, and cannot leave the
    /// document in either direction.
    #[test]
    fn source_scrolling_is_relative_and_clamped() {
        // Down from the anchor, then back above it.
        assert_eq!(source_anchor(60, Some(45), 20, 3), 38);
        assert_eq!(source_anchor(60, Some(45), 20, -5), 30);
        // Never above the top, however far up the user scrolls.
        assert_eq!(source_anchor(60, Some(45), 20, -9999), 0);
        // Never past the last screenful (60 rows, 20 tall → max offset 40).
        assert_eq!(source_anchor(60, Some(45), 20, 9999), 40);
        // A document shorter than the pane does not scroll at all.
        assert_eq!(source_anchor(5, Some(2), 20, 9999), 0);
    }

    /// A frame with no recoverable fault line (and the degenerate zero-height
    /// pane) must not panic or scroll somewhere arbitrary.
    #[test]
    fn the_anchor_handles_a_missing_fault_row_and_zero_height() {
        assert_eq!(source_anchor(60, None, 20, 0), 0);
        assert_eq!(
            source_anchor(60, None, 20, 5),
            5,
            "still scrollable by hand"
        );
        assert_eq!(source_anchor(0, None, 0, 0), 0);
        assert_eq!(source_anchor(60, Some(45), 0, 0), 0);
    }

    #[test]
    fn line_start_finds_each_line_offset() {
        let src = "aa\nbbb\nc";
        assert_eq!(line_start(src, 0), 0);
        assert_eq!(line_start(src, 1), 3);
        assert_eq!(line_start(src, 2), 7);
        assert_eq!(line_start(src, 99), src.len(), "past the end clamps");
    }

    #[test]
    fn line_index_counts_newlines_and_clamps() {
        let src = "a\nb\nc";
        assert_eq!(line_index(src, 0), 0);
        assert_eq!(line_index(src, 2), 1);
        assert_eq!(line_index(src, 4), 2);
        assert_eq!(line_index(src, 999), 2, "an offset past the end clamps");
    }

    #[test]
    fn span_line_is_one_based_and_rejects_the_null_span() {
        let src = "a\nb\nc";
        assert_eq!(
            span_line(src, &frame_with(Vec::new(), (0, 0))),
            None,
            "(0,0) is `no span recorded`"
        );
        assert_eq!(span_line(src, &frame_with(Vec::new(), (2, 3))), Some(2));
        assert_eq!(
            span_line(src, &frame_with(Vec::new(), (99, 100))),
            None,
            "out of range"
        );
    }

    #[test]
    fn digits_counts_decimal_width() {
        assert_eq!(digits(0), 1);
        assert_eq!(digits(9), 1);
        assert_eq!(digits(10), 2);
        assert_eq!(digits(1234), 4);
    }

    /// The overlay must stay inside a terminal smaller than its natural size,
    /// or it is drawn off-screen and the user sees a fragment.
    #[test]
    fn the_overlay_is_clamped_to_a_small_terminal() {
        let small = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 8,
        };
        let r = centered(small, 62, 24);
        assert!(r.width <= small.width && r.height <= small.height);
        assert!(r.x + r.width <= small.x + small.width);
        assert!(r.y + r.height <= small.y + small.height);
    }
}
