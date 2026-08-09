//! Breakpoint stops: what a `:bp` marker does at runtime (§9.8).
//!
//! A fault snapshot is taken while the stack is *unwinding* and is read after
//! every language frame has gone (see [`crate::crash_snapshot`]). A breakpoint is
//! the other case: the frames are still claimed, the program is still in the
//! middle of them, and the whole point is to go back afterwards. So this module
//! is deliberately not a second fault path — it is a call out to the host and a
//! return.
//!
//! ## The handler is given a snapshot and nothing else, and that is the design
//!
//! [`BreakpointHandler`] takes a `&`[`BreakpointStop`] and no `*mut
//! RuntimeContext`. It has no heap pointer, no allocator, no way to run
//! generated code — so **it cannot trigger a collection**, and that is a
//! structural fact rather than a rule a host has to remember.
//!
//! That matters because of what the stop hands over. [`BreakpointStop::frames`]
//! is a deep copy of the live debug chain, and its `GcRef`s are only as valid as
//! the objects they name. Values *below* the debug stack's `top` are ADR-106's
//! weak arm — every collection clears the slots whose objects it reclaimed — so
//! at the instant the copy is taken every reference in it is live. Keeping it
//! that way for the duration of the stop needs one thing: that no collection
//! happens while the host is looking. Withholding the context is how that is
//! guaranteed, and it is why the snapshot does not need to be registered as a
//! root set the way a *fault* snapshot does (which the host holds across
//! `restart`, `p EXPR` and everything else that allocates).
//!
//! ## Detaching, and why there is no `kill`
//!
//! [`Resume`] has two arms. `Continue` returns to the program. `Detach` returns
//! to the program *and disarms every later stop*, which is what "quit the
//! debugger" means from inside a live run.
//!
//! There is no third arm that ends the program, because there is nothing sound
//! to do with the frames: §9.2 forbids unwinding Rust through JIT frames, and
//! the fault epilogue that *can* unwind them is reached by raising a fault —
//! which would report the program as having failed when it did not. A host that
//! wants the run over can exit the process from its handler; the language will
//! not pretend a stop is a fault.

use std::cell::Cell;

use crate::crash_snapshot::CrashSnapshot;

/// One `:bp` stop, as the host sees it.
///
/// Carries no context, no heap and no way back into the runtime — see the module
/// header for why that absence is the point.
///
/// **Handed to the handler by value, and it must not survive the call.** The
/// `GcRef`s in [`frames`](Self::frames) name objects that are live *at the
/// instant of the stop*; the moment the handler returns, the program runs on and
/// the next allocation may collect any of them. Every other root set in the
/// system is registered with the collector, and this one deliberately is not —
/// the handler holds it for the length of one call, which is the whole window in
/// which no collection can happen (module header). A host that stashes it is
/// keeping a root set the collector cannot see.
pub struct BreakpointStop {
    /// The frame chain at the stop, innermost first: frame 0 is the function
    /// holding the marker, the last is the program's entry point.
    ///
    /// A [`CrashSnapshot`] because it is the same deep copy of the same debug
    /// chain, produced by the same walk. Its
    /// [`fault_kind`](CrashSnapshot::fault_kind) is
    /// [`FaultKind::None`](crate::FaultKind::None), which is the honest answer:
    /// nothing went wrong, the program stopped because it was asked to.
    pub frames: CrashSnapshot,
    /// The `:bp` marker's own source span `[start, end)`, so the host can point
    /// at the line the program stopped on rather than at the function's extent.
    pub span: (u32, u32),
    /// How many stops this run has made, counting this one. `1` on the first.
    ///
    /// The host's cheapest way to say "the tenth time round the loop" without
    /// keeping its own counter, and the runtime's already: the count is what
    /// decides whether a stop is the first one, which is the only thing the
    /// runtime itself does with it.
    pub hits: u64,
}

/// What the host wants done after a stop.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Resume {
    /// Return to the program. The next `:bp` stops again.
    Continue,
    /// Return to the program and stop at nothing else this run.
    Detach,
}

/// A host's breakpoint handler: given a stop, answers what to do next.
///
/// A plain `fn` for [`InputReader`](crate::InputReader)'s reason: it is stored
/// across the ABI boundary and called from generated code's stack, so it carries
/// no captured state and no lifetime. A host that needs state puts it in its own
/// thread-local, which is what `praxis-cli` does.
///
/// The stop arrives **by value** because the debugger the CLI builds from it
/// owns its snapshot, and it must be dropped before the handler returns — see
/// [`BreakpointStop`].
pub type BreakpointHandler = fn(BreakpointStop) -> Resume;

thread_local! {
    /// The installed handler, or none. Thread-local for
    /// [`crate::input`]'s reason: the runtime is single-threaded (§12.1) and
    /// there is one program per process.
    static HANDLER: Cell<Option<BreakpointHandler>> = const { Cell::new(None) };
    /// Stops made this run — [`BreakpointStop::hits`]'s source.
    static HITS: Cell<u64> = const { Cell::new(0) };
    /// Set by a [`Resume::Detach`]; makes every later stop a no-op until
    /// [`install_breakpoint_handler`] arms a fresh run.
    static DETACHED: Cell<bool> = const { Cell::new(false) };
}

/// Install the host's breakpoint handler, armed and with a fresh hit count.
///
/// A program compiled with `:bp` markers and run without one stops at nothing:
/// the marker's call finds no handler and returns, which is the right behaviour
/// for an embedder that has no debugger, and for every JIT test.
pub fn install_breakpoint_handler(handler: BreakpointHandler) {
    HANDLER.with(|slot| slot.set(Some(handler)));
    HITS.with(|slot| slot.set(0));
    DETACHED.with(|slot| slot.set(false));
}

/// Forget any installed handler, so every later `:bp` is a no-op.
///
/// The crash debugger's path needs this: a fault hands the terminal to the
/// debugger, and `restart` (§9.7) re-runs the program *from inside that screen*
/// — a stop handler firing there would take the terminal from the debugger
/// already holding it. Disarming before the hand-off is what keeps the two
/// surfaces from fighting over it.
pub fn clear_breakpoint_handler() {
    HANDLER.with(|slot| slot.set(None));
}

/// Whether a stop has detached (a [`Resume::Detach`] answer). The host's way to
/// tell a run the user walked away from apart from one that simply had no
/// markers left.
#[must_use]
pub fn breakpoints_detached() -> bool {
    DETACHED.with(Cell::get)
}

/// Take the stop: deep-copy the live debug chain, hand it to the installed
/// handler, and act on its answer.
///
/// # Safety
/// `ctx` must be live and wired, and every claimed debug frame entry must
/// satisfy `copy_stack`'s contract — which every generated prologue establishes.
pub(crate) unsafe fn stop(ctx: *mut crate::RuntimeContext, span: (u32, u32)) {
    if DETACHED.with(Cell::get) {
        return;
    }
    // Taken rather than borrowed for the duration, so a handler that somehow
    // re-entered this function finds none and returns instead of recursing.
    let Some(handler) = HANDLER.with(Cell::take) else {
        return;
    };
    let hits = HITS.with(|slot| {
        let n = slot.get().saturating_add(1);
        slot.set(n);
        n
    });
    // SAFETY: the caller guarantees `ctx` is live and wired.
    let frames = unsafe { crate::crash_snapshot::copy_live_chain(ctx) };
    let resume = handler(BreakpointStop { frames, span, hits });
    HANDLER.with(|slot| slot.set(Some(handler)));
    if resume == Resume::Detach {
        DETACHED.with(|slot| slot.set(true));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keep_going(_: BreakpointStop) -> Resume {
        Resume::Continue
    }

    /// Installing arms: a handler installed after a detached run stops again.
    #[test]
    fn installing_resets_the_detach_and_the_count() {
        DETACHED.with(|slot| slot.set(true));
        HITS.with(|slot| slot.set(7));
        assert!(breakpoints_detached());
        install_breakpoint_handler(keep_going);
        assert!(!breakpoints_detached());
        assert_eq!(HITS.with(Cell::get), 0);
        clear_breakpoint_handler();
    }

    /// A program run with no handler installed stops at nothing — the state an
    /// embedder that never asked for a debugger is in.
    #[test]
    fn no_handler_means_no_stop() {
        clear_breakpoint_handler();
        assert!(HANDLER.with(Cell::get).is_none());
    }
}
