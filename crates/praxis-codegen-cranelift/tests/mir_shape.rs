//! The MIR the backend is handed, counted from outside `praxis-mir`.
//!
//! This file exists to prove one thing about the machinery rather than
//! anything about the backend: `praxis_mir::test_support` is reachable from
//! another crate's tests, through `praxis-mir = { features = ["test-support"] }`
//! in this crate's `[dev-dependencies]` and nowhere else. Wave 0 built it
//! because the packages that verify their headline by counting instructions are
//! split across the MIR crate and this one (handover 26 §2), and a helper only
//! the MIR crate could reach would have been copied into this one within a wave.
//!
//! Keep it small. The census tests that assert about a *program* belong beside
//! the pass that changes it; what belongs here is the shape of the input the
//! Cranelift lowering has to emit code for.

use praxis_mir::ir::ScalarKind;
use praxis_mir::test_support::{lower_src_to_mir, Census, InstKind};

/// The reachability proof. Also the smallest true statement about the
/// backend's input: a float temporary reaches `lower_inst` as a
/// `Materialize{Float}`, which is the `praxis_alloc_float` call ADR-113's
/// interning does *not* cover — there is no table for a value space that is not
/// enumerable — and which handover 25 measured at 14% of `mandelbrot`.
///
/// **One box, where before ADR-120 there were two.** The builder writes one per
/// node of `a * b + b`; the block-local forwarding deletes the interior one,
/// because `a * b`'s box has exactly one consumer and that consumer is the
/// `+`'s reload in the same block. What reaches the backend is the boxes a
/// program's *results* need — here the returned sum — which is the point of
/// the pass and the reason this file's count is the honest one to assert.
#[test]
fn a_float_temporary_reaches_the_backend_as_a_materialize_of_a_float() {
    let lowered = lower_src_to_mir("fn f(a: Float, b: Float) -> Float { a * b + b }");
    let census = Census::of_function(lowered.function("f"));
    assert_eq!(
        census.count(InstKind::Materialize(ScalarKind::Float)),
        1,
        "the returned sum; `a * b`'s box was forwarded away: {census:?}"
    );
    assert_eq!(
        census.count(InstKind::Materialize(ScalarKind::Int)),
        0,
        "no integer is boxed here: {census:?}"
    );
}
