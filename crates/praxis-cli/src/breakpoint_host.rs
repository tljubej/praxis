//! The CLI's `:bp` stop handler (§9.8) — what happens when a running program
//! reaches a breakpoint marker.
//!
//! The runtime calls a plain `fn` with no captured state (see
//! [`praxis_runtime::BreakpointHandler`] for why), so everything the handler
//! needs to *render* a stop lives in this module's thread-local: the program's
//! type database, its source text and its file name. [`install`] fills that in
//! and arms the runtime; [`disarm`] takes it back down.
//!
//! ## Why the state is cloned rather than borrowed
//!
//! The handler runs several native frames *below* `run`, underneath the JIT
//! frames of the program itself, so there is no `&` to `run`'s locals to be had
//! — and reaching them through a raw pointer would alias the `Analysis` the
//! `DebugSession` may later take by value. A `TypeDb` clone is an append-only
//! arena copied once per run, against a program run that is about to do far
//! more work than that, and it preserves every positional `type_id` — which is
//! the property that matters, since a local's type is an *index* into the db
//! codegen used.
//!
//! ## Which surface a stop gets
//!
//! Exactly the fault path's rule (§9.6), asked in the same order, because a user
//! who has learned what `--debug` does to a crash should not have to learn a
//! second thing about a stop:
//!
//! - `--debug` declines (`never`, or `auto` off a terminal) → **no prompt**: the
//!   stop prints its frame and locals to stderr and the program continues. That
//!   turns `:bp` into a trace point, which is what a marker in a script or a
//!   CI run can usefully be — and it is what keeps a piped `praxis run` from
//!   having the debugger read the input the program was going to.
//! - `--debug` accepts and there is a terminal → the full-screen debugger.
//! - `--debug always` with no terminal → the `Praxis stop>` line prompt, reading
//!   commands from standard input. This is the form the book's sessions are
//!   driven with.

use std::cell::RefCell;

use praxis_debugger::repl::{Control, StoppedHost};
use praxis_runtime::{BreakpointStop, Resume};

use crate::color_mode::ColorMode;
use crate::debug_mode::DebugMode;

thread_local! {
    /// What the handler renders a stop with, installed before the run.
    static HOST: RefCell<Option<HostState>> = const { RefCell::new(None) };
}

/// The host state a stop is rendered against.
struct HostState {
    db: praxis_types::TypeDb,
    source_text: String,
    source_name: String,
    color: ColorMode,
    /// Whether `--debug` wants a prompt at all for this run. Which *kind* of
    /// prompt is asked separately, at the stop, exactly as the fault path asks
    /// it — see [`praxis_debugger::tui::should_use_tui`].
    prompt: bool,
}

/// Arm `:bp` stops for this run.
///
/// `db` must be the database codegen compiled against — see the module header.
/// A `--debug=never` run installs nothing at all, so its markers cost one call
/// each and print nothing, which is what "never" has to mean for a stop as much
/// as for a fault.
pub fn install(
    db: &praxis_types::TypeDb,
    source_text: &str,
    source_name: &str,
    debug: DebugMode,
    color: ColorMode,
) {
    if debug == DebugMode::Never {
        return;
    }
    HOST.with(|slot| {
        *slot.borrow_mut() = Some(HostState {
            db: db.clone(),
            source_text: source_text.to_string(),
            source_name: source_name.to_string(),
            color,
            prompt: debug.wants_repl(),
        })
    });
    praxis_runtime::install_breakpoint_handler(handle);
}

/// Disarm `:bp` stops and drop the host state.
///
/// Called before the crash debugger takes the terminal: `restart` (§9.7) re-runs
/// the program from inside that screen, and a stop firing there would take the
/// terminal from the debugger already holding it.
pub fn disarm() {
    praxis_runtime::clear_breakpoint_handler();
    HOST.with(|slot| *slot.borrow_mut() = None);
}

/// The installed handler. One stop, start to finish.
///
/// The `HOST` borrow is held across the whole stop, including the debugger's
/// event loop. That is safe here for the same reason the handler cannot collect:
/// nothing it can reach calls back into this module, so there is no second
/// borrow to conflict with.
fn handle(stop: BreakpointStop) -> Resume {
    HOST.with(|slot| {
        let borrow = slot.borrow();
        // A stop with no host state is a program the CLI armed and then
        // disarmed; nothing to render it with, so let it run.
        let Some(state) = borrow.as_ref() else {
            return Resume::Continue;
        };
        if !state.prompt {
            report(&stop, state);
            return Resume::Continue;
        }
        // The full-screen debugger when there is a terminal to take over, the
        // line prompt otherwise — `run`'s fault path decides between the two the
        // same way and for the same reasons, which is the point.
        if praxis_debugger::tui::should_use_tui() {
            show_tui(stop, state)
        } else {
            show_prompt(stop, state)
        }
    })
}

/// The noninteractive stop: print where the program is and what it holds, then
/// let it go. §9.6's shape, minus the exit — nothing has failed.
fn report(stop: &BreakpointStop, state: &HostState) {
    // The program's own output goes to stdout and this to stderr, so a reader
    // watching both sees them in the order they happened only if stdout is
    // flushed first.
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let ctx = praxis_debugger::render::RenderCtx::new(&state.db, &state.source_text);
    let _ = praxis_debugger::render::render_breakpoint_stop(
        &mut std::io::stderr(),
        &stop.frames,
        stop.span,
        stop.hits,
        state.color.palette(),
        &ctx,
    );
}

/// The full-screen stop: hand the frames to the debugger and wait.
fn show_tui(stop: BreakpointStop, state: &HostState) -> Resume {
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let span = stop.span;
    let repl = stopped_repl(stop, state);
    let tui = praxis_debugger::tui::Tui::new(repl, praxis_debugger::tui::Stop::Breakpoint);
    match praxis_debugger::tui::run(tui) {
        Ok((_, control)) => resume_for(control),
        // The terminal could not be taken. The frames went into the REPL and
        // are gone with it, so there is nothing left to render — say where the
        // program is from the marker's span and let it go, rather than leaving
        // it wedged against a screen that never appeared.
        Err(err) => {
            let _ = writeln!(
                std::io::stderr(),
                "error: could not open the debugger at the breakpoint \
                 (source bytes {}..{}): {err}",
                span.0,
                span.1
            );
            Resume::Continue
        }
    }
}

/// The line-prompt stop: read commands from standard input until one of them
/// lets the program go.
///
/// The prompt writes to **stderr**, like every other thing the debugger prints,
/// so a piped run's stdout is still the program's own output and nothing else.
fn show_prompt(stop: BreakpointStop, state: &HostState) -> Resume {
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let mut repl = stopped_repl(stop, state);
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    let stderr = std::io::stderr();
    let mut stderr = stderr.lock();
    let control = repl.run(&mut stdin, &mut stderr);
    let _ = stderr.flush();
    resume_for(control)
}

/// The REPL for one stop: the frames the runtime copied, plus what the host
/// lends it to render them with.
///
/// The snapshot moves into the REPL, which drops it when the stop ends — the
/// window [`BreakpointStop`]'s contract asks for.
fn stopped_repl(stop: BreakpointStop, state: &HostState) -> praxis_debugger::repl::Repl {
    let host = StoppedHost {
        db: state.db.clone(),
        source_text: state.source_text.clone(),
        source_name: state.source_name.clone(),
        hits: stop.hits,
        span: stop.span,
    };
    praxis_debugger::repl::Repl::new_stopped(stop.frames, host)
}

/// What a debugger surface's exit means for the program.
///
/// `quit` from a stop cannot end the program — §9.2 forbids unwinding Rust
/// through JIT frames, so there is no way out of the middle of a call — so it
/// means "stop debugging": run on, and stop at nothing else.
fn resume_for(control: Control) -> Resume {
    match control {
        Control::Quit => Resume::Detach,
        Control::Continue | Control::Resume => Resume::Continue,
    }
}
