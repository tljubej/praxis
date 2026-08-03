//! The process input buffer, read on **first use** (§7.1, §7.10).
//!
//! §7.10 is precise about when the read happens: "The first `read` lazily reads
//! standard input once into an immutable GC-managed source buffer; later `read`
//! expressions reuse it." The runtime had no way to express that — the host
//! read standard input to EOF *before* calling the entry function and installed
//! the buffer as [`RuntimeContext::input_source`], so a program with no `read`
//! at all still consumed stdin, and against an open pipe (a terminal, a CI
//! harness that leaves the descriptor open) `praxis run` blocked forever
//! (REP-51). Every `praxis run` of a `read`-free program hung.
//!
//! This module is the missing expression. The host installs a *reader* — it is
//! not called — and [`praxis_get_input`](crate::abi::praxis_get_input), which
//! is what a `read` lowers to first, calls it the one time. A program that
//! never evaluates a `read` never touches the host's input at all.
//!
//! **The reader is infallible by construction**, and deliberately so: what an
//! unreadable stdin *means* is the host's question, not the runtime's. The CLI
//! reports its own I/O failure the way it reports every other one. The runtime
//! is left with bytes, and the only judgement it makes about them is §4.3's:
//! text that is not UTF-8 is a fault. Since ADR-111 that judgement is made by
//! [`praxis_get_input`](crate::abi::praxis_get_input) itself rather than by
//! `praxis_alloc_text` — this is the one path in the runtime carrying bytes the
//! compiler did not produce, so it is the one place the check belongs, and
//! keeping it here is what leaves a `Text` *literal*'s allocation genuinely
//! non-faulting. The reader's contract is unchanged: bytes, infallibly.
//!
//! `praxis run` never reaches that fault, and it is worth knowing which caller
//! can. `lazy_stdin::read` (`praxis-cli/src/run.rs`) goes through
//! `std::io::read_to_string`, which refuses non-UTF-8 stdin and exits 2 before
//! the runtime sees a byte. `InvalidText` is therefore reachable only from an
//! embedder that installs an [`InputReader`] answering bytes of its own.
//!
//! The slot is thread-local because the runtime is single-threaded (§12.1) and
//! because a `static mut` would be worse; there is one program per process, so
//! there is one reader per process. A host that installs none — every JIT test,
//! every embedder — is indistinguishable from today: `praxis_get_input` finds
//! nothing to call and answers whatever `input_source` already holds.

use std::cell::Cell;

/// A host's process-input reader: the UTF-8 bytes of the input buffer.
///
/// Called at most once, from the first `read` a program evaluates. Its whole
/// obligation is bytes — however many the host has, including none. What becomes
/// of them, and why a zero-byte answer is still an input buffer, is stated once
/// at [`praxis_get_input`](crate::abi::praxis_get_input) (ADR-087).
///
/// A plain `fn` and not a closure: it is stored across the ABI boundary and
/// called from generated code's stack, so it carries no captured state and no
/// lifetime. A host that needs state puts it in its own thread-local, which is
/// what `praxis-cli` does.
pub type InputReader = fn() -> Vec<u8>;

thread_local! {
    static READER: Cell<Option<InputReader>> = const { Cell::new(None) };
}

/// Install the process-input reader. The host calls this **instead of** reading
/// its input up front; nothing here reads anything.
pub fn install_input_reader(reader: InputReader) {
    READER.with(|slot| slot.set(Some(reader)));
}

/// Forget any installed reader, so the next `read` finds the buffer the host
/// installed directly rather than calling back.
///
/// The crash debugger's re-run path (§9.7) needs this: it re-installs the
/// *same* input on each restart to keep re-runs identical, and a reader that
/// fired again would read a stdin that is now at EOF.
pub fn clear_input_reader() {
    READER.with(|slot| slot.set(None));
}

/// Take the installed reader, leaving none. Taking rather than borrowing is
/// what makes "once" structural: there is no second call to make.
pub(crate) fn take_input_reader() -> Option<InputReader> {
    READER.with(Cell::take)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nothing() -> Vec<u8> {
        Vec::new()
    }

    /// The reader is taken, not borrowed, so a second `read` cannot re-read.
    #[test]
    fn a_reader_is_taken_once_and_then_gone() {
        clear_input_reader();
        assert!(take_input_reader().is_none());
        install_input_reader(nothing);
        assert!(take_input_reader().is_some());
        assert!(take_input_reader().is_none());
    }

    /// Clearing an installed reader is what the debugger's re-run path does.
    #[test]
    fn clearing_disarms_an_installed_reader() {
        install_input_reader(nothing);
        clear_input_reader();
        assert!(take_input_reader().is_none());
    }
}
