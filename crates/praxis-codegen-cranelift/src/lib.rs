//! Cranelift JIT code generation and ABI lowering (§10, §14.1).
//!
//! Responsibility (per the design): lower MIR to Cranelift IR, register runtime
//! symbols (`praxis_alloc`, `praxis_int_add`, `praxis_vec_push`, ...), and
//! finalize the JIT module. Every generated function follows the uniform
//! `fn(RuntimeContext*, GcRef...) -> GcRef` convention (§10.3).
//!
//! **Milestone 0: skeleton.** The `cranelift` dependency is intentionally
//! absent — it is added in Milestone 4 when the first code is generated.

/// Marker documenting that this crate is a deliberate skeleton.
pub const FILLED_AT_MILESTONE: u32 = 4;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skeleton_reports_fill_milestone() {
        assert_eq!(FILLED_AT_MILESTONE, 4);
    }
}
