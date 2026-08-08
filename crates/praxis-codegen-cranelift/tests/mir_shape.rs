//! The MIR the backend is handed, counted from outside `praxis-mir`.
//!
//! This file exists to prove one thing about the machinery rather than
//! anything about the backend: `praxis_mir::test_support` is reachable from
//! another crate's tests, through `praxis-mir = { features = ["test-support"] }`
//! in this crate's `[dev-dependencies]` and nowhere else. The tests that verify
//! a change by counting instructions are split across the MIR crate and this
//! one, and a helper only the MIR crate could reach would be copied into this
//! one instead.
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
/// enumerable — and which measures at 14% of `mandelbrot`.
///
/// **One box, not two.** The builder writes one per node of `a * b + b`;
/// ADR-120's block-local forwarding deletes the interior one, because `a * b`'s
/// box has exactly one consumer and that consumer is the `+`'s reload in the
/// same block. What reaches the backend is the boxes a program's *results* need
/// — here the returned sum — which is the point of the pass and the reason this
/// file's count is the honest one to assert.
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

/// **The sample loop proves no scalar's descriptor at all.**
///
/// Every `ExtractScalar` of a wired scalar is one `emit_scalar_load`, and
/// `emit_scalar_load` is the language's only descriptor-proof emitter, so this
/// census *is* the site count.
///
/// The count is zero because two passes each remove a share of it. ADR-120's
/// block-local forwarding removes the box/unbox pair at every interior node of
/// the expression trees, and the whole `Materialize{Bool}` →
/// `ExtractScalar{Bool}` → `Branch` shape of the `while` condition. ADR-121's
/// promotion removes what the rest read from: `i`, `acc` and `limit` are
/// `Scalar` slots, so there is no object whose descriptor could be proved. The
/// loop body is `ConstInt`, `IntBinOp`, `IntCmp`, `CheckFault` and `MoveScalar`
/// and nothing else.
///
/// **ADR-116's headline therefore has no denominator left in this loop.** That
/// trade — three ALU operations for one L1 load *per descriptor proof* — is
/// worth zero at zero proofs. It is not worth zero everywhere: a proof site
/// survives wherever the value came out of the runtime, which `provable`'s
/// suite census counts at 122 sites across the eight benchmarks. Any figure
/// quoted "per iteration of the sample loop" is a figure about a loop that does
/// not contain the thing being measured.
#[test]
fn the_sample_loop_proves_no_scalars_descriptor_at_all() {
    let lowered = lower_src_to_mir(
        "var i = 0\n\
         var acc = 0\n\
         var limit = 10\n\
         while i < limit {\n\
         \x20   acc = acc + i * 3\n\
         \x20   i = i + 1\n\
         }\n\
         out(acc)\n",
    );
    let entry = lowered.entry();
    let census = lowered
        .innermost_loop_over(entry, "acc + i * 3")
        .census(entry);

    // Straight-line body, single-expression condition: every block of the
    // region runs on every iteration, so the region's static count is the
    // per-iteration count.
    assert_eq!(
        census.count(InstKind::ExtractScalar(ScalarKind::Int)),
        0,
        "ADR-120 forwarded away the three interior nodes of the expression \
         trees, and ADR-121 removed the other five by promoting `i`, `acc` and \
         `limit` to `Scalar` slots — there is no object left to read: {census:?}"
    );
    assert_eq!(
        census.count(InstKind::ExtractScalar(ScalarKind::Bool)),
        0,
        "the `while` condition used to unbox the `Bool` it had just boxed; \
         ADR-120's terminator rewrite means it never boxes it: {census:?}"
    );
    // The other half of the same claim, and the control for it: a census that
    // counts zero reloads because the loop stopped *computing* rather than
    // stopped boxing would pass the assertion above and be a catastrophe.
    assert_eq!(
        census.count(InstKind::IntBinOp),
        3,
        "`i * 3`, `acc + …` and `i + 1` — the arithmetic is untouched: {census:?}"
    );
    assert_eq!(
        census.count(InstKind::Materialize(ScalarKind::Int)),
        0,
        "and nothing is boxed on the way back: {census:?}"
    );
    assert_eq!(
        census.count(InstKind::CheckFault),
        3,
        "three fallible operations, and this number does *not* move — ADR-120 \
         forwards no producer that can fault, so the fold ADR-117 measured has \
         the same count of checks to fold as it did: {census:?}"
    );
    assert_eq!(
        census.count(InstKind::ExtractScalar(ScalarKind::Char))
            + census.count(InstKind::ExtractScalar(ScalarKind::Float))
            + census.count(InstKind::ExtractScalar(ScalarKind::Byte)),
        0,
        "an integer loop proves no other scalar; a proof site of a kind with \
         no inline form would not be a proof at all: {census:?}"
    );
}
