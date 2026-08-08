//! What descriptor a `Gc` local provably holds, when MIR itself emitted it
//! (ADR-122).
//!
//! ADR-102 made every scalar payload read a *branch*: the generated code loads
//! the object's descriptor, compares it against the one the payload's width
//! belongs to, and diverts to a refusal when they disagree. That check exists
//! because [`Inst::ExtractScalar`] names a width — `praxis_int_load` reads eight
//! bytes — and a MIR that names the wrong one is an out-of-bounds heap read from
//! a program `praxis check` accepted (REP-56, and REP-49 before it).
//!
//! This module is the *static* half of the same question, and it is an
//! **analysis, not a transform**: nothing here rewrites an instruction. For each
//! [`LocalKind::Gc`] local it answers "which [`DescriptorClass`] does every
//! definition of this slot produce", over the lattice
//!
//! ```text
//!                       Top  — no definition seen
//!     Int Bool Char Float Unit Text Record Tuple Enum Closure Collection
//!                      Bottom — no proof
//! ```
//!
//! and [`crate::verify`] refuses an `ExtractScalar` whose width contradicts the
//! answer.
//!
//! # What is a producer, and what is not
//!
//! The producers are the three shapes where **MIR wrote the descriptor down**:
//! [`Inst::ConstGc`] (whose four variants name an interned `Int`, an interned
//! `Char`, the `Unit` singleton and the two `Bool` singletons),
//! [`Inst::Alloc`] (whose
//! [`AllocKind`] *is* the class), and [`Inst::Materialize`] (whose
//! [`ScalarKind`] is). [`Inst::MoveGc`] resolves to its source. Everything else
//! that writes a `Gc` local — `Call`, `CallIndirect`, `LoadField`,
//! `LoadTupleElem`, `EnumPayloadGet`, `LoadCapture` — is `Bottom`: the
//! descriptor came out of the runtime, and reading the *static* type off
//! [`crate::ir::Local::ty`] instead would be believing the front end's
//! guarantee, which is the premise handover 26 §4 read the repair log and
//! refuted.
//!
//! **The analysis is flow-insensitive and that is what makes it sound over a
//! non-SSA IR.** The answer is a property of the *slot* — "every definition
//! anywhere in the function produces `K`" — so it holds at every point that
//! reads the slot without any dominance or reaching-definitions question, which
//! is exactly the four analyses ADR-108 declined to build. It is also what makes
//! `MoveGc` chasing sound: if every definition of `src` is a `K`, then a copy of
//! `src` is a `K` wherever the copy happens.
//!
//! # Two traps, both of which ship silently unsound if missed
//!
//! **A local with no defining instruction is `Bottom`, permanently.** Function
//! parameters are `Gc` locals the builder creates with a real
//! [`MirType::Known`](crate::ir::MirType::Known) and no defining instruction, as
//! are closure-prologue captures. "Every definition is a `K`-producer" is
//! *vacuously true over an empty set*, so a universal quantifier written the
//! obvious way blesses every parameter with whatever class the reader hoped for
//! — at exactly the site where it is most tempting, `primes`' `is_prime(n)`,
//! whose two `ExtractScalar{Int}`s read a parameter. The meet is never taken
//! over an empty set here: [`ProvableDescriptors::of`] seeds a definition-less
//! local `Bottom` and the fixpoint skips it, so there is no identity element to
//! get wrong.
//!
//! **The fixpoint is a *greatest* fixpoint** — optimistic start at `Top`,
//! iterate down to convergence. A pessimistic start is not merely imprecise, it
//! is wrong for the shape this exists for: `Bottom` is absorbing, so a loop
//! variable whose back edge assigns it from another loop variable resolves to
//! `Bottom` on the first pass and can never climb back out. The two-variable
//! swap in `a_pair_of_loop_variables_that_define_each_other_is_still_provable`
//! is the smallest program that shows it.
//!
//! # What it does not answer
//!
//! `Bottom` is not a defect and must never be reported as one. It is the honest
//! answer for every value that came out of the runtime, which is most of a
//! program that touches collections at all — see [`crate::verify`]'s
//! [`ProvedDescriptorMismatch`](crate::VerifyError::ProvedDescriptorMismatch)
//! for the rule this licenses and the reason it refuses only a *proved
//! contradiction*.

use crate::ir::{AllocKind, Function, GcConst, Inst, LocalId, LocalKind, ScalarKind};
use crate::verify::defines;

/// The kind of object a descriptor describes — one class per shape the
/// runtime's descriptor table distinguishes at the granularity MIR can name.
///
/// This is deliberately **not** [`praxis_types::Type`]: two `Record`s with
/// different fields are one class here, because the question this answers is
/// "would an [`Inst::ExtractScalar`] of width `K` be reading the wrong object",
/// and every record is the wrong object for every scalar width. Refining it
/// would buy nothing the rule can spend.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum DescriptorClass {
    Int,
    Bool,
    Char,
    Float,
    Unit,
    Text,
    Record,
    Tuple,
    Enum,
    Closure,
    Collection,
}

impl DescriptorClass {
    /// The class of the object an [`Inst::ExtractScalar`] or
    /// [`Inst::Materialize`] of this payload width belongs to.
    ///
    /// `None` for [`ScalarKind::Byte`], and for
    /// [`ScalarKind::BYTE_HAS_NO_WRAPPER`]'s reason: there is no
    /// `praxis_alloc_byte` row, so there is no descriptor and no class. The
    /// absence propagates as an absence of proof rather than as `Int`'s class,
    /// which is what the two panicking arms in `ir.rs` refuse to do.
    #[inline]
    #[must_use]
    pub const fn of_scalar(scalar: ScalarKind) -> Option<DescriptorClass> {
        match scalar {
            ScalarKind::Int => Some(DescriptorClass::Int),
            ScalarKind::Bool => Some(DescriptorClass::Bool),
            ScalarKind::Char => Some(DescriptorClass::Char),
            ScalarKind::Float => Some(DescriptorClass::Float),
            ScalarKind::Byte => None,
        }
    }

    /// The class an [`Inst::Alloc`] mints. Total: every [`AllocKind`] names
    /// exactly one shape, which is why the allocation is a producer at all.
    #[inline]
    #[must_use]
    const fn of_alloc(alloc: &AllocKind) -> DescriptorClass {
        match alloc {
            AllocKind::Int { .. } => DescriptorClass::Int,
            AllocKind::Bool { .. } => DescriptorClass::Bool,
            AllocKind::Unit => DescriptorClass::Unit,
            AllocKind::Text { .. } => DescriptorClass::Text,
            AllocKind::Char { .. } => DescriptorClass::Char,
            AllocKind::Float { .. } => DescriptorClass::Float,
            AllocKind::Record { .. } => DescriptorClass::Record,
            AllocKind::Enum { .. } => DescriptorClass::Enum,
            AllocKind::Tuple { .. } => DescriptorClass::Tuple,
            AllocKind::Closure { .. } => DescriptorClass::Closure,
            AllocKind::Collection { .. } => DescriptorClass::Collection,
        }
    }

    /// The class of an immortal the runtime minted before the program started.
    /// [`GcConst`] has four variants and each names its own descriptor, which
    /// is the whole reason a constant is a producer.
    #[inline]
    #[must_use]
    const fn of_gc_const(konst: GcConst) -> DescriptorClass {
        match konst {
            GcConst::SmallInt(_) => DescriptorClass::Int,
            GcConst::Unit => DescriptorClass::Unit,
            GcConst::Bool(_) => DescriptorClass::Bool,
            GcConst::Char(_) => DescriptorClass::Char,
        }
    }

    /// The class's name as a diagnostic renders it.
    #[inline]
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            DescriptorClass::Int => "Int",
            DescriptorClass::Bool => "Bool",
            DescriptorClass::Char => "Char",
            DescriptorClass::Float => "Float",
            DescriptorClass::Unit => "Unit",
            DescriptorClass::Text => "Text",
            DescriptorClass::Record => "Record",
            DescriptorClass::Tuple => "Tuple",
            DescriptorClass::Enum => "Enum",
            DescriptorClass::Closure => "Closure",
            DescriptorClass::Collection => "Collection",
        }
    }
}

impl std::fmt::Display for DescriptorClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// One point of the lattice. **Private, and that is the second half of trap 1.**
///
/// [`Lattice::Top`] means "no definition has been folded in yet", which during
/// the fixpoint is an *assumption* and never an answer. The only way out of this
/// module is [`ProvableDescriptors::class`], which returns
/// `Option<DescriptorClass>` — so a caller cannot receive `Top` and cannot
/// mistake it for a proof, whatever it hoped the slot held.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Lattice {
    /// No definition seen. The identity of [`Lattice::meet`].
    Top,
    /// Every definition folded in so far produces this class.
    Known(DescriptorClass),
    /// No proof: two producers disagree, or a producer is not one of the four
    /// shapes MIR writes the descriptor at.
    Bottom,
}

impl Lattice {
    /// The greatest lower bound: two producers that agree keep the class, two
    /// that disagree have nothing in common, and `Bottom` absorbs.
    #[inline]
    fn meet(self, other: Lattice) -> Lattice {
        match (self, other) {
            (Lattice::Bottom, _) | (_, Lattice::Bottom) => Lattice::Bottom,
            (Lattice::Top, x) | (x, Lattice::Top) => x,
            (Lattice::Known(a), Lattice::Known(b)) => {
                if a == b {
                    Lattice::Known(a)
                } else {
                    Lattice::Bottom
                }
            }
        }
    }
}

/// What one instruction contributes about the local it defines.
///
/// The three cases are the whole of the analysis, and separating them from the
/// fixpoint means the [`Inst`] match runs once per instruction rather than once
/// per instruction per iteration.
#[derive(Clone, Copy, Debug)]
enum Def {
    /// MIR emitted the descriptor here, so the class is read straight off the
    /// instruction.
    Produces(DescriptorClass),
    /// [`Inst::MoveGc`]: whatever the source resolves to, by fixpoint.
    CopyOf(LocalId),
    /// The class comes from outside MIR's own emissions — a runtime call, a
    /// field read, a scalar destination that has no descriptor at all.
    Opaque,
}

/// What each instruction says about the local it writes.
///
/// **The `_` arm is deliberate and its failure mode is one-directional.** Every
/// other exhaustive match over [`Inst`] in this crate has a wrong answer
/// available to it; this one does not. A variant added later and not named here
/// contributes [`Def::Opaque`], which meets to [`Lattice::Bottom`], which is an
/// *absence of proof* — so the cost of forgetting is a missed elision, never a
/// verifier that blesses a read it should have refused. That is worth more than
/// keeping ADR-044's count of exhaustive matches at five would have been, and it
/// is the same trade handover 27 §3 makes for W8-S0's rollback guard.
fn def_of(inst: &Inst) -> Option<(LocalId, Def)> {
    let dst = defines(inst)?;
    let def = match inst {
        Inst::ConstGc { konst, .. } => Def::Produces(DescriptorClass::of_gc_const(*konst)),
        Inst::Alloc { alloc, .. } => Def::Produces(DescriptorClass::of_alloc(alloc)),
        // `None` is `ScalarKind::Byte`, which has no descriptor to prove.
        Inst::Materialize { scalar, .. } => match DescriptorClass::of_scalar(*scalar) {
            Some(class) => Def::Produces(class),
            None => Def::Opaque,
        },
        Inst::MoveGc { src, .. } => Def::CopyOf(*src),
        _ => Def::Opaque,
    };
    Some((dst, def))
}

/// The provable descriptor class of every local in one function.
///
/// Built by [`ProvableDescriptors::of`] in one pass plus a fixpoint; read by
/// [`ProvableDescriptors::class`], which is the only accessor and answers
/// `Option<DescriptorClass>` rather than a lattice point.
#[derive(Clone, Debug)]
pub struct ProvableDescriptors {
    /// Indexed by [`LocalId`], parallel to `Function::locals`.
    classes: Vec<Lattice>,
}

impl ProvableDescriptors {
    /// Analyze `func`.
    ///
    /// One linear pass to collect each local's definitions, then a greatest
    /// fixpoint over them. The lattice has height three and values only descend,
    /// so it converges in at most `2 * locals + 1` rounds — the bound is stated
    /// because it is the reason no worklist is needed, not because it is tight.
    #[must_use]
    pub fn of(func: &Function) -> ProvableDescriptors {
        let n = func.locals.len();
        let mut defs: Vec<Vec<Def>> = vec![Vec::new(); n];
        for block in &func.blocks {
            for inst in &block.insts {
                if let Some((dst, def)) = def_of(inst) {
                    // An out-of-range destination is `VerifyError::LocalOutOfRange`'s
                    // to report; this pass must not index past the table to find it.
                    if let Some(slot) = defs.get_mut(dst.0 as usize) {
                        slot.push(def);
                    }
                }
            }
        }

        // **Trap 1, made structural.** A local with no definitions never enters
        // the fixpoint below, so the meet there is never taken over an empty
        // set and there is no identity element to mistake for an answer: the
        // seed says `Bottom` in so many words. A `Scalar` local is seeded the
        // same way for a different reason — it holds a raw payload word, not a
        // reference, so it has no descriptor to prove (ADR-015 §10.3).
        let mut classes: Vec<Lattice> = (0..n)
            .map(|i| {
                if defs[i].is_empty() || func.locals[i].kind != LocalKind::Gc {
                    Lattice::Bottom
                } else {
                    Lattice::Top
                }
            })
            .collect();

        loop {
            let mut changed = false;
            for i in 0..n {
                // Nothing to recompute: no definitions (seeded `Bottom`), or
                // already at the lattice's floor, which is absorbing.
                if defs[i].is_empty() || classes[i] == Lattice::Bottom {
                    continue;
                }
                let next = defs[i].iter().fold(Lattice::Top, |acc, def| {
                    acc.meet(match def {
                        Def::Produces(class) => Lattice::Known(*class),
                        Def::CopyOf(src) => classes
                            .get(src.0 as usize)
                            .copied()
                            .unwrap_or(Lattice::Bottom),
                        Def::Opaque => Lattice::Bottom,
                    })
                });
                if next != classes[i] {
                    classes[i] = next;
                    changed = true;
                }
            }
            if !changed {
                return ProvableDescriptors { classes };
            }
        }
    }

    /// The class every definition of `local` produces, or `None` when there is
    /// no proof — which includes a local with no definition at all, a local
    /// whose producers disagree, and every local the runtime filled.
    ///
    /// A [`LocalId`] past the end of the table answers `None` rather than
    /// panicking: it is malformed MIR, and
    /// [`VerifyError::LocalOutOfRange`](crate::VerifyError::LocalOutOfRange) is
    /// the rule that says so.
    #[inline]
    #[must_use]
    pub fn class(&self, local: LocalId) -> Option<DescriptorClass> {
        match self.classes.get(local.0 as usize) {
            Some(Lattice::Known(class)) => Some(*class),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annot::{DebugSlots, RootSlots};
    use crate::ir::fixtures::{gc_local, scalar_local};
    use crate::ir::{BlockId, Function, Inst, ScalarKind, Terminator};

    fn push(f: &mut Function, blk: BlockId, inst: Inst) {
        f.blocks[blk.0 as usize].insts.push(inst);
    }

    /// The simplest producer: an immortal read out of the context names its own
    /// descriptor, so the local holding it is proved.
    #[test]
    fn a_const_gc_proves_the_class_of_the_immortal_it_names() {
        let mut f = Function::empty("f");
        let (i, u, b) = (gc_local(&mut f), gc_local(&mut f), gc_local(&mut f));
        f.return_local = i;
        let blk = f.new_block();
        push(
            &mut f,
            blk,
            Inst::ConstGc {
                dst: i,
                konst: GcConst::SmallInt(7),
            },
        );
        push(
            &mut f,
            blk,
            Inst::ConstGc {
                dst: u,
                konst: GcConst::Unit,
            },
        );
        push(
            &mut f,
            blk,
            Inst::ConstGc {
                dst: b,
                konst: GcConst::Bool(true),
            },
        );
        f.blocks[blk.0 as usize].term = Terminator::Return { value: i };

        let p = ProvableDescriptors::of(&f);
        assert_eq!(p.class(i), Some(DescriptorClass::Int));
        assert_eq!(p.class(u), Some(DescriptorClass::Unit));
        assert_eq!(p.class(b), Some(DescriptorClass::Bool));
    }

    /// An allocation's [`AllocKind`] *is* the class, and a `Materialize`'s
    /// [`ScalarKind`] is — the other two producers, in one function.
    #[test]
    fn an_alloc_and_a_materialize_each_prove_their_own_class() {
        let mut f = Function::empty("f");
        let payload = scalar_local(&mut f, ScalarKind::Float);
        let allocated = gc_local(&mut f);
        let materialized = gc_local(&mut f);
        f.return_local = allocated;
        let blk = f.new_block();
        push(
            &mut f,
            blk,
            Inst::ConstFloat {
                dst: payload,
                bits: 0,
            },
        );
        push(
            &mut f,
            blk,
            Inst::Alloc {
                dst: allocated,
                alloc: AllocKind::Float { value: payload },
                roots: RootSlots::unannotated(),
                debug: DebugSlots::unannotated(),
            },
        );
        push(
            &mut f,
            blk,
            Inst::Materialize {
                dst: materialized,
                src: payload,
                scalar: ScalarKind::Float,
                roots: RootSlots::unannotated(),
                debug: DebugSlots::unannotated(),
            },
        );
        f.blocks[blk.0 as usize].term = Terminator::Return { value: allocated };

        let p = ProvableDescriptors::of(&f);
        assert_eq!(p.class(allocated), Some(DescriptorClass::Float));
        assert_eq!(p.class(materialized), Some(DescriptorClass::Float));
        assert_eq!(
            p.class(payload),
            None,
            "a Scalar slot holds a raw word, so it has no descriptor to prove"
        );
    }

    /// **Trap 1.** A parameter is a `Gc` local with a real static type and *no
    /// defining instruction*, so "every definition produces `Int`" is vacuously
    /// true over an empty set. The answer must be `None`.
    ///
    /// This is the one way the rule ships silently unsound, and it is unsound at
    /// the most tempting site in the suite: `primes`' `is_prime(n)` reads its
    /// parameter with two `ExtractScalar{Int}`s, and blessing them is exactly
    /// what a naive universal quantifier does.
    #[test]
    fn a_parameter_has_no_definition_and_is_therefore_not_provable() {
        let mut f = Function::empty("f");
        let param = gc_local(&mut f);
        let out = gc_local(&mut f);
        f.params = vec![param];
        f.return_local = out;
        let blk = f.new_block();
        // The parameter is *read* here, which is what makes the absence of a
        // definition the interesting case rather than a dead slot.
        push(
            &mut f,
            blk,
            Inst::MoveGc {
                dst: out,
                src: param,
            },
        );
        f.blocks[blk.0 as usize].term = Terminator::Return { value: out };

        let p = ProvableDescriptors::of(&f);
        assert_eq!(p.class(param), None, "no definition is no proof");
        assert_eq!(
            p.class(out),
            None,
            "and the absence propagates through the copy"
        );
    }

    /// **Trap 2.** Two loop variables that define each other across a back edge
    /// are provable only under a *greatest* fixpoint. A single forward pass — or
    /// any pessimistic start — reports `Bottom` for both, because `Bottom` is
    /// absorbing and nothing climbs back out of it.
    ///
    /// The shape is `var a = 0; var b = 1; while … { var t = a; a = b; b = t }`,
    /// with the two `MoveGc`s on the back edge that a loop-carried assignment
    /// lowers to.
    #[test]
    fn a_pair_of_loop_variables_that_define_each_other_is_still_provable() {
        let mut f = Function::empty("f");
        let a = gc_local(&mut f);
        let b = gc_local(&mut f);
        let t = gc_local(&mut f);
        let seed_a = gc_local(&mut f);
        let seed_b = gc_local(&mut f);
        let cond = scalar_local(&mut f, ScalarKind::Bool);
        f.return_local = a;
        let entry = f.new_block();
        let body = f.new_block();
        let exit = f.new_block();

        push(
            &mut f,
            entry,
            Inst::ConstGc {
                dst: seed_a,
                konst: GcConst::SmallInt(0),
            },
        );
        push(
            &mut f,
            entry,
            Inst::MoveGc {
                dst: a,
                src: seed_a,
            },
        );
        push(
            &mut f,
            entry,
            Inst::ConstGc {
                dst: seed_b,
                konst: GcConst::SmallInt(1),
            },
        );
        push(
            &mut f,
            entry,
            Inst::MoveGc {
                dst: b,
                src: seed_b,
            },
        );
        push(
            &mut f,
            entry,
            Inst::ConstInt {
                dst: cond,
                value: 1,
            },
        );
        f.blocks[entry.0 as usize].term = Terminator::Branch {
            cond,
            then_block: body,
            else_block: exit,
        };

        // The back edge: `t = a; a = b; b = t`.
        push(&mut f, body, Inst::MoveGc { dst: t, src: a });
        push(&mut f, body, Inst::MoveGc { dst: a, src: b });
        push(&mut f, body, Inst::MoveGc { dst: b, src: t });
        f.blocks[body.0 as usize].term = Terminator::Jump { target: entry };
        f.blocks[exit.0 as usize].term = Terminator::Return { value: a };

        let p = ProvableDescriptors::of(&f);
        assert_eq!(p.class(a), Some(DescriptorClass::Int), "a is an Int");
        assert_eq!(p.class(b), Some(DescriptorClass::Int), "so is b");
        assert_eq!(p.class(t), Some(DescriptorClass::Int), "and the swap temp");
    }

    /// Two producers that disagree meet to no proof — the case the rule exists
    /// to distinguish from a proved contradiction, because a slot written by an
    /// `Int` on one arm and a `Unit` on the other has no single class.
    #[test]
    fn two_producers_that_disagree_meet_to_no_proof() {
        let mut f = Function::empty("f");
        let slot = gc_local(&mut f);
        f.return_local = slot;
        let blk = f.new_block();
        push(
            &mut f,
            blk,
            Inst::ConstGc {
                dst: slot,
                konst: GcConst::SmallInt(1),
            },
        );
        push(
            &mut f,
            blk,
            Inst::ConstGc {
                dst: slot,
                konst: GcConst::Unit,
            },
        );
        f.blocks[blk.0 as usize].term = Terminator::Return { value: slot };

        assert_eq!(ProvableDescriptors::of(&f).class(slot), None);
    }

    /// A value that came out of the runtime is `Bottom`, and this is the reason
    /// the rule catches REP-56 and REP-49 but **not** REP-54 or TY-31's catalog
    /// bound: both of those build their wrong descriptor inside a wrapper, and
    /// MIR sees only an [`Inst::Call`] with a `Gc` destination.
    #[test]
    fn a_call_result_is_not_provable_because_the_runtime_chose_its_descriptor() {
        let mut f = Function::empty("f");
        let out = gc_local(&mut f);
        f.return_local = out;
        let blk = f.new_block();
        push(
            &mut f,
            blk,
            Inst::Call {
                dst: out,
                callee: crate::ir::CallTarget::User("g".into()),
                args: Vec::new(),
                roots: RootSlots::unannotated(),
                debug: DebugSlots::unannotated(),
            },
        );
        push(
            &mut f,
            blk,
            Inst::CheckFault {
                on_fault: blk,
                debug: DebugSlots::unannotated(),
            },
        );
        f.blocks[blk.0 as usize].term = Terminator::Return { value: out };

        assert_eq!(ProvableDescriptors::of(&f).class(out), None);
    }

    /// A chain of copies resolves to the class at its root, however long, and a
    /// chain whose root is opaque resolves to nothing. This is the property the
    /// census depends on: `TypedExpr::Path` hands back the variable's slot, so
    /// every read of a user variable is one `MoveGc` away from its producer.
    #[test]
    fn a_move_gc_chain_resolves_to_the_class_at_its_root() {
        for (root, expected) in [(true, Some(DescriptorClass::Bool)), (false, None)] {
            let mut f = Function::empty("f");
            let head = gc_local(&mut f);
            let mid = gc_local(&mut f);
            let tail = gc_local(&mut f);
            f.return_local = tail;
            let blk = f.new_block();
            if root {
                push(
                    &mut f,
                    blk,
                    Inst::ConstGc {
                        dst: head,
                        konst: GcConst::Bool(false),
                    },
                );
            } else {
                push(
                    &mut f,
                    blk,
                    Inst::LoadField {
                        dst: head,
                        src: head,
                        field_idx: 0,
                    },
                );
                push(
                    &mut f,
                    blk,
                    Inst::CheckFault {
                        on_fault: blk,
                        debug: DebugSlots::unannotated(),
                    },
                );
            }
            push(
                &mut f,
                blk,
                Inst::MoveGc {
                    dst: mid,
                    src: head,
                },
            );
            push(
                &mut f,
                blk,
                Inst::MoveGc {
                    dst: tail,
                    src: mid,
                },
            );
            f.blocks[blk.0 as usize].term = Terminator::Return { value: tail };

            assert_eq!(ProvableDescriptors::of(&f).class(tail), expected);
        }
    }
}

/// The census handover 27 §5 makes the gate for W11's *backend* half — how
/// often the analysis above actually proves something on the benchmark suite.
///
/// It lives here rather than in a scratch script because the number is a claim
/// about this tree and it expires the moment the lowering changes. Both columns
/// are reported, and reporting both is the point: handover 26's producer set
/// omits [`Inst::MoveGc`], and `TypedExpr::Path` hands back the *binding's slot*
/// — so every read of a user variable is an `ExtractScalar` whose `src` is
/// `MoveGc`-defined and the literal column covers none of them. Run the census
/// one way and the package is declined on an artifact of how one sentence was
/// written.
#[cfg(test)]
mod census {
    use super::*;
    use crate::ir::{BlockId, Function};
    // The forwarded door (ADR-121, and `forward.rs`'s test module for the full
    // reason). Every figure in this census is documented as a *post-W8-S0*
    // measurement — "the post-W8-S0 inner-loop census, to the site" — so it must
    // read the MIR that description names.
    use crate::test_support::lower_src_to_mir_forwarded as lower_src_to_mir;
    use crate::test_support::{benchmark_source, Lowered, BENCHMARK_SUITE};

    /// How many `ExtractScalar` sites in a region are provable, in both columns.
    #[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
    struct Sites {
        total: usize,
        /// Handover 26's set: every definition of `src` is *directly* a
        /// `ConstGc`, an `Alloc` or a `Materialize`.
        literal: usize,
        /// The same question with `MoveGc` resolved transitively — which is what
        /// [`ProvableDescriptors`] answers.
        chased: usize,
    }

    impl Sites {
        fn pct(n: usize, d: usize) -> String {
            if d == 0 {
                "  n/a".to_string()
            } else {
                format!("{:5.1}", 100.0 * n as f64 / d as f64)
            }
        }

        fn row(self, label: &str) -> String {
            format!(
                "{label:<20} {:>4} sites   literal {:>4} ({}%)   chased {:>4} ({}%)",
                self.total,
                self.literal,
                Sites::pct(self.literal, self.total),
                self.chased,
                Sites::pct(self.chased, self.total),
            )
        }

        fn add(&mut self, other: Sites) {
            self.total += other.total;
            self.literal += other.literal;
            self.chased += other.chased;
        }
    }

    /// Whether every definition of `local` is one of handover 26's three
    /// producers *without* chasing a copy. Trap 1 applies to this column too: a
    /// local with no definition is not literal-provable either, which is why
    /// this ends `seen.is_some()` rather than `true`.
    fn literally_provable(func: &Function, local: LocalId) -> bool {
        let mut seen: Option<DescriptorClass> = None;
        for block in &func.blocks {
            for inst in &block.insts {
                let Some((dst, def)) = def_of(inst) else {
                    continue;
                };
                if dst != local {
                    continue;
                }
                match def {
                    Def::Produces(class) if seen.is_none() || seen == Some(class) => {
                        seen = Some(class);
                    }
                    _ => return false,
                }
            }
        }
        seen.is_some()
    }

    /// Census the `ExtractScalar` sites in the named blocks of one function.
    fn sites_in(func: &Function, blocks: impl IntoIterator<Item = BlockId>) -> Sites {
        let provable = ProvableDescriptors::of(func);
        let mut sites = Sites::default();
        for id in blocks {
            for inst in &func.blocks[id.0 as usize].insts {
                if let Inst::ExtractScalar { src, .. } = inst {
                    sites.total += 1;
                    sites.literal += usize::from(literally_provable(func, *src));
                    sites.chased += usize::from(provable.class(*src).is_some());
                }
            }
        }
        sites
    }

    /// Every `ExtractScalar` in every function the module lowers.
    fn sites_in_module(lowered: &Lowered) -> Sites {
        let mut total = Sites::default();
        for func in &lowered.funcs {
            total.add(sites_in(func, (0..func.blocks.len() as u32).map(BlockId)));
        }
        total
    }

    /// **The suite-wide census, both columns.** Run with `--nocapture` to read
    /// the table.
    ///
    /// The assertion is on the *gap between the columns*, not on the digits: the
    /// digits move with every lowering change — W8-S0 deletes sites from the
    /// denominator, W4b adds inline arms — and a gate that has to be re-typed is
    /// a gate nobody re-reads. The gap is the finding, and it is the one thing
    /// that cannot be an artifact of the benchmark selection.
    ///
    /// Measured **before** W8-S0 merged: 419 sites, 230 literal (54.9%), 340
    /// chased (81.1%), with `vm` the floor at 54.3% and `collatz` the ceiling at
    /// 95.8%.
    ///
    /// Measured with W8-S0 and W8-S0b in the tree and W4b not yet: 219 sites,
    /// 30 literal (13.7%), 140 chased (63.9%). The denominator nearly halved
    /// and the literal column collapsed by 41 points, and both are one fact:
    /// W8-S0's producer set is `Materialize`/`Alloc`/`ConstGc`, which is
    /// *exactly* what the literal column counts, so the two anti-correlate
    /// rather than merely differ. Every site the literal column could prove is
    /// a site W8-S0 would rather delete.
    ///
    /// That is the strongest argument in the round against W11's backend half,
    /// and it arrived by measurement rather than by judgement.
    ///
    /// Measured **on this tree**, with W4b in it too: **218 sites, 30 literal
    /// (13.8%), 140 chased (64.2%)**. The whole of the move is **one site, in
    /// `bfs`** — 63 → 62, with `literal` and `chased` both unchanged at 4 and
    /// 49 — and the size of it is the finding. ADR-122's open questions
    /// nominate W4b as the way to make *more* sites provable, on the grounds
    /// that it "moves descriptors out of `Inst::Call` and into MIR's own
    /// emissions". That is true of exactly one of its three arms:
    /// `Inst::BitsetContains` is a MIR instruction and its `ExtractScalar`
    /// disappears with the box W8-S0 forwards away, which is the one site.
    /// `praxis_vec_get` and `praxis_vec_len` inline **in the backend** and
    /// keep their `Inst::Call` in MIR, so this census — which reads MIR —
    /// cannot see them, and the descriptor of a value the wrapper minted is no
    /// more provable than it was. Making it so is an `Inst` per primitive, not
    /// a backend arm (ADR-118 decision 10).
    #[test]
    fn the_census_over_the_whole_suite_is_a_different_answer_in_each_column() {
        let mut suite = Sites::default();
        println!("\nExtractScalar sites, whole module, per benchmark:");
        for name in BENCHMARK_SUITE {
            let lowered = lower_src_to_mir(&benchmark_source(name));
            let sites = sites_in_module(&lowered);
            println!("  {}", sites.row(name));
            suite.add(sites);
        }
        println!("  {}\n", suite.row("SUITE"));

        assert!(
            suite.chased * 10 >= suite.total * 6,
            "the chased column clears three fifths of the suite: {suite:?}"
        );
        assert!(
            (suite.chased - suite.literal) * 5 >= suite.total,
            "and it is at least twenty points above the literal one, which is \
             the whole reason both are reported: {suite:?}"
        );
    }

    /// **The inner-loop census, and the two hand counts it settles.**
    ///
    /// Handover 27 §5 hand-walked `collatz`/`primes`/`mandelbrot` inner loops
    /// twice. Its **pre-W8-S0** count — 29/56 = 52% literal, 54/56 = 96% chased
    /// — was **exact on all four numbers**, verified mechanically before W8-S0
    /// merged. Its **post-W8-S0 estimate** — 12/39 and 37/39 — was **wrong in
    /// both**, and this test now holds the measured answer: **29 sites, 2
    /// literal (6.9%), 27 chased (93.1%)**.
    ///
    /// Both errors point the same way, which is why they matter. The denominator
    /// is 29 rather than 39 because W8-S0 deletes ten more sites than the walk
    /// expected, and the literal column is 6.9% rather than 31% because W8-S0's
    /// producer set *is* the literal column's — so it eats its own evidence.
    ///
    /// **One word of handover 27 is also wrong.** It glosses the pre-W8-S0 52%
    /// as "a fail on the 'fewer than half' gate". 29 of 56 is not fewer than
    /// half; it is a bare *pass*, by one site. What genuinely fails handover
    /// 26's gate is this tree's 6.9%.
    ///
    /// **W4b moves none of these four numbers**, and that is not a null result
    /// worth shrugging at: `collatz`, `primes` and `mandelbrot` are arithmetic
    /// loops that touch no `BitSet` and no `Vec`, so the one site W4b removes
    /// suite-wide lands in `bfs` and nowhere near here. The three inner loops
    /// stay at 29/2/27 exactly.
    ///
    /// These digits move with every lowering change. Re-measure, do not re-type:
    /// run with `--nocapture` and the table prints itself.
    #[test]
    fn the_inner_loop_census_measures_what_two_hand_counts_only_estimated() {
        let cases: [(&str, Option<&str>, &str); 3] = [
            ("collatz", None, "3 * c + 1"),
            ("primes", Some("is_prime"), "n % d == 0"),
            ("mandelbrot", None, "x * x - y * y + x0"),
        ];
        let mut total = Sites::default();
        println!("\nExtractScalar sites, innermost loop, per benchmark:");
        for (name, func_name, needle) in cases {
            let lowered = lower_src_to_mir(&benchmark_source(name));
            let func = match func_name {
                Some(n) => lowered.function(n),
                None => lowered.entry(),
            };
            let region = lowered.innermost_loop_over(func, needle);
            let sites = sites_in(func, region.blocks.iter().copied());
            println!("  {}", sites.row(name));
            total.add(sites);
        }
        println!("  {}\n", total.row("THREE INNER LOOPS"));

        assert_eq!(
            (total.total, total.literal, total.chased),
            (29, 2, 27),
            "the post-W8-S0 inner-loop census, to the site"
        );
    }
}
