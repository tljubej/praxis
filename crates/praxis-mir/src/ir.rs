//! The mid-level IR data structures (§13.5, ADR-015).
//!
//! MIR is deliberately **not SSA**: it is a sea of basic blocks operating over
//! named [`Local`] slots. Every language value lives in a [`Local`] of kind
//! [`LocalKind::Gc`] (holding a uniform `GcRef`); transient scalar payloads
//! (an `i64` loaded out of an `Int` object for a local computation) live in a
//! [`LocalKind::Scalar`] and **must not** survive a GC safepoint — the lowering
//! materializes a fresh `GcRef` from them before any safepoint, call, store, or
//! return (§10.3). The Cranelift backend turns this slot-based CFG into SSA.
//!
//! The fault protocol (§10.4) is woven in: a [`Inst::CheckFault`] tests the
//! context's `pending_fault` and diverts to a [`Terminator::Fault`] edge when a
//! runtime wrapper reports overflow / division-by-zero.

#![allow(dead_code)] // Some variants/fields are consumed by the Cranelift backend
                     // (praxis-codegen-cranelift) which lands later in M4.

use praxis_types::Type;

/// A whole lowered function: its locals, basic blocks, and debug metadata.
#[derive(Debug)]
pub struct Function {
    /// A stable, human-readable name (the source `fn` name). Used to derive the
    /// JIT symbol and for diagnostics.
    pub name: String,
    /// The parameter locals, in declaration order. Each is a `Gc` slot.
    pub params: Vec<LocalId>,
    /// The return local (a `Gc` slot holding the returned `GcRef`).
    pub return_local: LocalId,
    /// Every local declared in the function, indexed by `LocalId`.
    pub locals: Vec<Local>,
    /// The basic blocks, indexed by `BlockId`. Block 0 is the entry.
    pub blocks: Vec<Block>,
    /// Source-name metadata per local, for fault snapshots (§19 M4 acceptance:
    /// "named locals are available as `GcRef` values in fault snapshots").
    pub debug_names: Vec<Option<String>>,
}

/// A local slot.
#[derive(Debug)]
pub struct Local {
    pub id: LocalId,
    /// What the slot holds. Governs how the backend lays it out and whether the
    /// GC must see it at a safepoint.
    pub kind: LocalKind,
    /// The static language type (best-effort; `Scalar` payloads may carry a more
    /// precise `ScalarKind` than the `Type` admits).
    pub ty: Type,
}

/// What a [`Local`] holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalKind {
    /// A uniform `GcRef` to a GC object. These are the only locals the GC roots
    /// at a safepoint (§12.3).
    Gc,
    /// A transient scalar payload extracted from a `GcRef` for a local
    /// computation. Must be rematerialized into a `GcRef` before any safepoint,
    /// call, store, or return (§10.3).
    Scalar(ScalarKind),
}

/// The representation of a transient scalar payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScalarKind {
    /// `i64` — the payload of an `Int` object.
    Int,
    /// `u8` — the payload of a `Byte` object (reserved; M4 focuses on Int).
    Byte,
    /// `u32` — the payload of a `Char` object (reserved).
    Char,
    /// `bool` — the payload of a `Bool` object (represented as `i8`/`u8`).
    Bool,
}

/// A basic block: a straight-line list of instructions and one terminator.
#[derive(Debug)]
pub struct Block {
    pub id: BlockId,
    pub insts: Vec<Inst>,
    pub term: Terminator,
}

/// An instruction. Operands and results are [`Local`]s (slot-based, not SSA).
#[derive(Debug)]
pub enum Inst {
    /// Load an immediate integer constant into a `Scalar(Int)` local.
    ConstInt { dst: LocalId, value: i64 },
    /// Allocate a fresh `GcRef` and store it in `dst`. This is a GC safepoint
    /// (allocation may trigger collection). `kind` selects the runtime wrapper.
    Alloc {
        dst: LocalId,
        alloc: AllocKind,
        /// The set of `Gc` locals live across this safepoint — filled by the
        /// liveness pass. The backend spills exactly these into the shadow stack.
        live_roots: Vec<LocalId>,
    },
    /// Read a scalar payload out of a `Gc` local into a `Scalar` local.
    ExtractScalar {
        dst: LocalId,
        src: LocalId,
        scalar: ScalarKind,
    },
    /// Write a scalar payload from a `Scalar` local into a `Gc` local's object.
    /// (Re-materializes the object reference first if needed.)
    StoreScalar {
        dst_gc: LocalId,
        src: LocalId,
        scalar: ScalarKind,
    },
    /// Materialize a `GcRef` from a `Scalar` payload (`Scalar` → `Gc`). This is
    /// a safepoint (the allocation may trigger collection).
    Materialize {
        dst: LocalId,
        src: LocalId,
        scalar: ScalarKind,
        live_roots: Vec<LocalId>,
    },
    /// A checked binary arithmetic op on `Int` scalars. On overflow the runtime
    /// sets `pending_fault`; the following [`Inst::CheckFault`] diverts.
    IntBinOp {
        op: IntBinOp,
        dst: LocalId,
        lhs: LocalId,
        rhs: LocalId,
    },
    /// A comparison yielding a `Bool` scalar (`i8`).
    IntCmp {
        op: CmpOp,
        dst: LocalId,
        lhs: LocalId,
        rhs: LocalId,
    },
    /// Call a function. Arguments and result are `Gc` locals. A safepoint +
    /// fault check follow (calls may allocate and may fault).
    Call {
        dst: LocalId,
        callee: CallTarget,
        args: Vec<LocalId>,
        live_roots: Vec<LocalId>,
    },
    /// Test `pending_fault`; if set, jump to `on_fault`. Inserted after any
    /// faultable operation (checked arith, div/rem, calls).
    CheckFault { on_fault: BlockId },
    /// Copy one `Gc` local into another (a move; no allocation).
    MoveGc { dst: LocalId, src: LocalId },
}

/// What to allocate, for [`Inst::Alloc`].
#[derive(Debug)]
pub enum AllocKind {
    /// A boxed `Int` initialized from `value` (a `Scalar` local).
    Int { value: LocalId },
    /// A boxed `Bool` initialized from `value` (a `Scalar` local).
    Bool { value: LocalId },
    /// The `Unit` singleton (an immortal; no allocation, but still a safepoint
    /// shape for uniformity).
    Unit,
    /// A boxed `Text` from a literal (the string is embedded in the MIR).
    Text { value: String },
}

/// A call target. M4 resolves user functions by name; the backend mints a symbol.
#[derive(Clone, Debug)]
pub enum CallTarget {
    /// A user-defined function, by its MIR-local index-resolved name. The
    /// backend resolves this to the JIT'd function pointer.
    User(String),
    /// A built-in runtime wrapper, by its `praxis_*` symbol name (M5, §11.1).
    /// The backend resolves this through the registered symbol table. Method
    /// calls (`receiver.push(x)`) lower to this variant.
    Runtime(String),
}

/// Checked integer binary operators (§4.12). All fault on overflow; `Div`/`Rem`
/// additionally fault on division-by-zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntBinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

/// Comparison operators (yield `Bool`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Neq,
    Lt,
    Gt,
    Le,
    Ge,
}

/// A block terminator.
#[derive(Debug)]
pub enum Terminator {
    /// Conditional branch on a `Bool` scalar local.
    Branch {
        cond: LocalId,
        then_block: BlockId,
        else_block: BlockId,
    },
    /// Unconditional jump.
    Jump { target: BlockId },
    /// Return the `GcRef` in `value`.
    Return { value: LocalId },
    /// A fault edge: control reached here because `pending_fault` was set. The
    /// backend unwinds to the host (no Rust panic — §10.4).
    Fault,
}

// ---------------------------------------------------------------------------
// Opaque ids (index-based, `Copy`).
// ---------------------------------------------------------------------------

/// A local-slot id (index into [`Function::locals`]).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct LocalId(pub u32);

/// A basic-block id (index into [`Function::blocks`]).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct BlockId(pub u32);

impl Function {
    /// Append a new local, returning its id.
    pub fn new_local(&mut self, kind: LocalKind, ty: Type, debug_name: Option<String>) -> LocalId {
        let id = LocalId(self.locals.len() as u32);
        self.locals.push(Local { id, kind, ty });
        self.debug_names.push(debug_name);
        id
    }

    /// Append a new empty block that jumps nowhere yet.
    pub fn new_block(&mut self) -> BlockId {
        let id = BlockId(self.blocks.len() as u32);
        self.blocks.push(Block {
            id,
            insts: Vec::new(),
            term: Terminator::Jump { target: id }, // placeholder; overwritten.
        });
        id
    }

    /// The debug name for a local, if any.
    pub fn debug_name(&self, local: LocalId) -> Option<&str> {
        self.debug_names
            .get(local.0 as usize)
            .and_then(Option::as_deref)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_local_and_block_increment_ids() {
        let mut f = Function {
            name: "f".into(),
            params: Vec::new(),
            return_local: LocalId(0),
            locals: Vec::new(),
            blocks: Vec::new(),
            debug_names: Vec::new(),
        };
        let a = f.new_local(LocalKind::Gc, Type(0), Some("x".into()));
        let b = f.new_local(LocalKind::Scalar(ScalarKind::Int), Type(0), None);
        let blk = f.new_block();
        assert_eq!(a, LocalId(0));
        assert_eq!(b, LocalId(1));
        assert_eq!(blk, BlockId(0));
        assert_eq!(f.debug_name(a), Some("x"));
        assert_eq!(f.debug_name(b), None);
    }
}
