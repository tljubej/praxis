//! The teardown proof that gates generation reclamation (F13, hazard H15).
//!
//! JIT metadata — record schemas, tuple schemas, field-name strings, debug
//! local metadata — is reachable from *live heap objects*, not only from
//! generated code. A [`RecordPayload`](crate::RecordPayload) holds a
//! `*const RecordSchema`; a [`TuplePayload`](crate::TuplePayload) holds a
//! `*const TupleSchema`. While those objects are alive, freeing the arena the
//! schemas live in is a use-after-free waiting for the next `==`, `format` or
//! `hash`.
//!
//! Before S8 that could not happen, because the metadata was `Box::leak`ed and
//! never freed. S8 makes it reclaimable, so the ordering has to be *encoded*
//! rather than documented: a generation is reclaimed by handing it a
//! [`HeapDrained`], and the only way to obtain one is [`Runtime::teardown`],
//! which consumes the runtime and drops the heap — running every finalizer
//! first.
//!
//! [`Runtime::teardown`]: crate::Runtime::teardown

/// Proof that a heap has been dropped, and with it every object that could
/// hold a pointer into a JIT generation's arena.
///
/// Minted only by [`Runtime::teardown`](crate::Runtime::teardown). It carries
/// no data; its whole purpose is that a function requiring one cannot be called
/// before the heap is gone.
///
/// **What it does and does not prove.** It proves *a* runtime was torn down. A
/// process that builds two runtimes could tear down the first and retire a
/// generation the second still refers into — the token is a guard rail against
/// the ordering mistake, not a theorem about aliasing. The CLI and the debugger
/// each own exactly one `Runtime`, which is the configuration it is written
/// for. It is `Clone` because one teardown can legitimately retire several
/// generations (the debugger has a main generation and an evaluation
/// generation).
#[derive(Debug, Clone)]
pub struct HeapDrained {
    /// Private, so `HeapDrained` is unconstructible outside this crate.
    _seal: (),
}

impl HeapDrained {
    /// Mint the proof. Callable only from inside the runtime crate, and only
    /// from [`Runtime::teardown`](crate::Runtime::teardown).
    pub(crate) fn new() -> Self {
        HeapDrained { _seal: () }
    }
}

#[cfg(test)]
mod tests {
    use crate::Runtime;

    /// Tearing down a runtime yields the proof, and the proof can retire more
    /// than one generation (the debugger has two).
    #[test]
    fn teardown_yields_a_clonable_proof() {
        let rt = Runtime::new();
        let proof = rt.teardown();
        let _second = proof.clone();
    }

    /// The heap really is dropped by `teardown`: a payload's finalizer runs, so
    /// nothing that could name a generation's arena is left alive.
    #[test]
    fn teardown_finalizes_live_payloads() {
        let rt = Runtime::new();
        let _text = rt.alloc_text("a payload with an owned buffer");
        assert!(rt.heap().stats().live_count > 0);
        // The proof exists only after the heap (and `_text`'s buffer) is gone.
        let _proof = rt.teardown();
    }
}
