//! The runtime input-parser interpreter (§7, M6).
//!
//! Evaluates a compiled [`ParserPlan`] against the process-input buffer (or a
//! `Text` value), allocating GC results (`Int`, `Char`, source-slice `Text`,
//! `Vec`, `Grid`, `Record`) and raising `FaultKind::ParseFailed` on mismatch.
//!
//! The plan type and global slab live in `praxis-input-parser`; this interpreter
//! looks up plans by index and walks their `#[repr(C)]` node arena. WS7 fills in
//! the full interpreter; the entry point is [`run_plan_by_index`], called by the
//! `praxis_run_parser` ABI wrapper.

use crate::context::RuntimeContext;
use crate::text::text_bytes;
use crate::text::TextPayload;
use crate::GcRef;

/// Run the parser plan identified by `index` (from the global slab in
/// `praxis-input-parser`) against `input` (a `Text` GcRef), returning the parsed
/// result or `None` on failure.
///
/// Returns `None` (which the ABI wrapper turns into a `ParseFailed` fault) if
/// the plan index is out of range. A parse mismatch also returns `None` after
/// setting the fault.
///
/// # Safety
/// `ctx` must be live and wired; `input` must be a valid `Text` GcRef.
pub unsafe fn run_plan_by_index(
    ctx: *mut RuntimeContext,
    index: u32,
    input: GcRef,
) -> Option<GcRef> {
    let plan = praxis_input_parser::get_plan(index)?;
    // SAFETY: caller guarantees ctx/input validity.
    Some(unsafe { run_plan(ctx, plan, input) })
}

/// Run a parser plan against an input buffer. Allocates GC results through the
/// heap; on a parse mismatch, sets `FaultKind::ParseFailed` and returns the Unit
/// sentinel (the caller's ABI wrapper checks the fault).
///
/// # Safety
/// `ctx` must be live and wired; `input` must be a valid `Text` GcRef.
unsafe fn run_plan(
    ctx: *mut RuntimeContext,
    plan: &praxis_input_parser::ParserPlan,
    input: GcRef,
) -> GcRef {
    // Read the input bytes from the Text GcRef.
    let payload = input.payload::<TextPayload>();
    let bytes = unsafe { text_bytes(payload) };
    // Walk the plan from the root node.
    match walk(ctx, plan, plan.root, bytes) {
        ParseResult::Value(v) => v,
        ParseResult::Fault => unsafe { fault_sentinel(ctx) },
    }
}

/// The outcome of parsing a region: either a produced value or a fault.
#[allow(dead_code)] // WS7 fills in the interpreter that constructs Value.
enum ParseResult {
    Value(GcRef),
    Fault,
}

/// Set a `ParseFailed` fault and return the sentinel (used by the ABI wrapper).
unsafe fn fault_sentinel(ctx: *mut RuntimeContext) -> GcRef {
    // The ABI wrapper sets the fault; here we just return the Unit sentinel.
    // Re-use the runtime's unit_ref field.
    unsafe { (*ctx).unit_ref }
}

/// Walk a plan node against a byte slice, producing a value or faulting.
///
/// WS7 fills in the real interpreter (atomics, constructors, templates). For
/// now this returns `Fault` so the pipeline compiles end-to-end without
/// producing incorrect results — it is replaced in WS7.
unsafe fn walk(
    _ctx: *mut RuntimeContext,
    _plan: &praxis_input_parser::ParserPlan,
    _node: u32,
    _bytes: &[u8],
) -> ParseResult {
    ParseResult::Fault
}

#[cfg(test)]
mod tests {
    // WS7 fills these in with the interpreter + extensive unit tests.
    #[test]
    fn parser_module_present() {
        // Placeholder so the module compiles; real tests arrive in WS7.
    }
}
