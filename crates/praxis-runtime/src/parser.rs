//! The runtime input-parser interpreter (§7, M6).
//!
//! Evaluates a compiled [`ParserPlan`] against the process-input buffer (or a
//! `Text` value), allocating GC results (`Int`, `Char`, source-slice `Text`,
//! `Vec`, `Grid`, `Record`) and raising `FaultKind::ParseFailed` on mismatch.
//!
//! This module is filled in WS7. The plan type lives in `praxis-input-parser`
//! (compile-time); the interpreter here consumes it through the `#[repr(C)]`
//! `PlanNode` shape, so `praxis-runtime` depends on `praxis-input-parser` only
//! for that type (no rowan dependency).

#[cfg(test)]
mod tests {
    // WS7 fills this in with the interpreter + extensive unit tests.
    #[test]
    fn parser_module_present() {
        // Placeholder so the module compiles; real tests arrive in WS7.
    }
}
