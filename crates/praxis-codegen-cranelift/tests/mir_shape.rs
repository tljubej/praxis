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

/// **Handover 25 §3's loop has five type-proof sites per iteration. It had
/// nine, and handover 25 said seven.**
///
/// Every `ExtractScalar` of a wired scalar is one `emit_scalar_load`, and
/// `emit_scalar_load` is the language's only descriptor-proof emitter, so this
/// census *is* the site count.
///
/// ### Why the number moved, and what it was
///
/// The three answers are three different trees, and each was right on its own:
///
/// | tree | proofs per iteration |
/// |---|---:|
/// | handover 25 §3, hand-counted at `e4f42e6` | 7 (wrong) |
/// | this census when W6 and W7 wrote it (ADR-116, ADR-117) | **9** |
/// | this tree, with ADR-120's forwarding merged | **5** |
///
/// Nine was the count of what `build.rs` *emits* — eight `ExtractScalar{Int}`
/// (two operands each for `i * 3`, `acc + …`, `i + 1` and `i < limit`) plus the
/// condition's `ExtractScalar{Bool}`. ADR-120 part 1 then deleted four of them:
/// the box/unbox pair at every interior node of the expression trees, and the
/// whole `Materialize{Bool}` → `ExtractScalar{Bool}` → `Branch` shape of the
/// `while` condition, which is why the `Bool` line below is now zero rather
/// than one.
///
/// **This is the double count handover 21 §3.6 recorded and handover 26 §7 trap
/// 7 warned about**, and it arrived as a failing test rather than as a wrong
/// number in a report — which is the wave structure working. W6 and W7 each
/// independently counted nine and each pinned it, correcting handover 25; both
/// were right, and neither could see that the package beside them in the same
/// wave was about to delete four of the nine. The amendments in ADR-116 and
/// ADR-117 restate their per-iteration figures against this denominator.
///
/// The count is asserted rather than printed because it is load-bearing twice
/// over: it is the denominator of ADR-116's headline, and a later package that
/// removes proof sites (W11's backend half) has to be measured against a figure
/// that was checked rather than remembered.
#[test]
fn the_sample_loop_proves_a_scalars_descriptor_five_times_per_iteration() {
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
        5,
        "three of the builder's eight `Int` reloads were interior nodes of an \
         expression tree and are forwarded away (ADR-120): {census:?}"
    );
    assert_eq!(
        census.count(InstKind::ExtractScalar(ScalarKind::Bool)),
        0,
        "the `while` condition used to unbox the `Bool` it had just boxed; \
         ADR-120's terminator rewrite means it never boxes it: {census:?}"
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
