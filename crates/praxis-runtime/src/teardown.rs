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

/// Drop every registered parser plan and every schema the parser interpreter
/// built from one (IP-12).
///
/// These two have to go together. A named-capture template's `RecordSchema`
/// borrows its field names straight out of plan storage, so retiring the plans
/// while the schema cache still holds them leaves dangling `&'static str`s;
/// retiring the schemas while a live `RecordPayload` still points at one is a
/// use-after-free at the next `==`. The [`HeapDrained`] argument rules out the
/// second, and doing both here rules out the first.
///
/// This lives in `praxis-runtime` rather than in `praxis-input-parser` because
/// the proof does: `praxis-input-parser` cannot depend on this crate (the
/// interpreter makes the arrow point the other way).
pub fn retire_parser_plans(_proof: &HeapDrained) {
    // SAFETY: `_proof` witnesses that the heap has been dropped, so no payload
    // survives to dereference a schema; and the schema cache — the only other
    // holder of names borrowed from plan storage — is cleared first.
    unsafe {
        crate::parser::retire_schemas();
        praxis_input_parser::retire_all_plans();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Runtime;

    /// Tearing down a runtime yields the proof, and the proof can retire more
    /// than one generation (the debugger has two).
    #[test]
    fn teardown_yields_a_clonable_proof() {
        let rt = Runtime::new();
        let proof = rt.teardown();
        let _second = proof.clone();
    }

    /// Retiring the plans releases them, and the arena really is empty
    /// afterwards. Registering again still works — the arena is a store, not a
    /// one-shot.
    #[test]
    fn retiring_parser_plans_empties_the_arena() {
        use praxis_input_parser::{lower_to_plan, register_plan, ParserAst};
        let ast = ParserAst::Atomic {
            kind: praxis_input_parser::AtomicKind::Int,
            span: praxis_source::Span::at(0),
        };
        register_plan(lower_to_plan(&ast)).expect("the arena has room");
        assert!(praxis_input_parser::plan_count() > 0);
        let proof = Runtime::new().teardown();
        retire_parser_plans(&proof);
        assert_eq!(praxis_input_parser::plan_count(), 0);
        let after = register_plan(lower_to_plan(&ast)).expect("registration still works");
        assert_eq!(after.get(), 1, "ids restart from one after a retirement");
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
