//! The JIT module: owns the Cranelift `JITModule`, declares/defines functions,
//! and hands back callable entry pointers (§10, §10.5).
//!
//! One [`Jit`] owns one generation of compiled code (§10.5). M4 uses a single
//! generation per `run`. The `praxis_*` runtime symbols are registered through
//! `JITBuilder::symbol` so the JIT resolves imported calls without a linker.

use std::collections::HashMap;
use std::rc::Rc;

use anyhow::anyhow;
use cranelift::codegen::isa::CallConv;
use cranelift::prelude::{AbiParam, Signature};
use cranelift_frontend::FunctionBuilderContext;
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId, Linkage, Module};
use praxis_mir::Function as MirFunction;
use praxis_runtime::{GcRef, HeapDrained, RuntimeContext};

use crate::generation::Generation;
use crate::lower;
use crate::symbols;

/// A compiled, callable entry point: `fn(*mut RuntimeContext) -> GcRef`.
///
/// This is exactly what `lower::abi_signature` emits for a zero-parameter
/// function such as `main`: the hidden context pointer and nothing else. It
/// previously declared a trailing `GcRef` the generated code never had, and
/// every caller invented a value to fill it — an ABI mismatch that happened to
/// be harmless on the supported host. A function *with* parameters is called
/// through its own transmuted type (see the debugger's `call_with_arity`).
pub type RunnableFunction = unsafe extern "C" fn(*mut RuntimeContext) -> GcRef;

/// Errors that can arise during JIT compilation. Variants box their payloads
/// to keep the enum small (clippy::result_large_err).
#[derive(Debug, thiserror::Error)]
pub enum JitError {
    #[error("host target unsupported by Cranelift: {0}")]
    UnsupportedTarget(String),
    #[error("Cranelift error: {0}")]
    Cranelift(#[from] Box<anyhow::Error>),
    #[error("module error: {0}")]
    Module(#[from] Box<cranelift_module::ModuleError>),
}

/// Owns one JIT generation: the `JITModule` (code + data memory), the
/// per-thread `FunctionBuilderContext` reused across lowered functions, and the
/// [`Generation`] arena that owns every piece of metadata the generated code
/// and the runtime reach by raw pointer (F13).
///
/// **Field order is load-bearing.** Rust drops fields in declaration order, so
/// `module` — the executable code holding arena addresses as immediates — is
/// torn down before `generation`. (Dropping a generation does not free its
/// arena anyway; see [`Generation::retire`]. The order still states the
/// intent, and it is what makes an eventual `Drop`-based reclamation correct.)
pub struct Jit {
    module: JITModule,
    fn_ctx: FunctionBuilderContext,
    generation: Rc<Generation>,
}

impl Jit {
    /// Create a fresh JIT with the `praxis_*` symbols registered, in its own
    /// new generation.
    ///
    /// # Errors
    /// Returns [`JitError::UnsupportedTarget`] if the host CPU isn't supported,
    /// or if its pointer width or endianness is not the one the lowering
    /// assumes (see [`Jit::check_target`]).
    pub fn new() -> Result<Self, JitError> {
        Self::in_generation(Rc::new(Generation::new()))
    }

    /// Create a fresh JIT that compiles into an *existing* generation.
    ///
    /// The debugger uses this: every `p EXPR` compiles a throwaway module, but
    /// they all share the session's one arena, so the metadata they mint is
    /// interned instead of accumulating (DBG-05). The values a `p` leaves in
    /// the heap keep pointing at schemas the shared generation still owns.
    ///
    /// # Errors
    /// As [`Jit::new`].
    pub fn in_generation(generation: Rc<Generation>) -> Result<Self, JitError> {
        let mut builder = JITBuilder::new(cranelift_module::default_libcall_names())
            .map_err(|e| JitError::UnsupportedTarget(format!("{e:?}")))?;
        // Resolve `praxis_*` imports through `symbols::resolve` — the one
        // table. The previous code kept a second, hand-maintained list of 57
        // names here; it had already drifted from the ~130 the resolver knows,
        // and the omissions were invisible because the JIT falls back to
        // `dlsym`, which finds the statically linked runtime anyway.
        builder.symbol_lookup_fn(Box::new(symbols::resolve));
        let module = JITModule::new(builder);
        Self::check_target(module.isa().pointer_type(), module.isa().endianness())?;
        Ok(Jit {
            module,
            fn_ctx: FunctionBuilderContext::new(),
            generation,
        })
    }

    /// A handle on this JIT's generation, for a host that wants to compile a
    /// second module into the same arena or to retire it later.
    pub fn generation(&self) -> Rc<Generation> {
        Rc::clone(&self.generation)
    }

    /// Tear down this JIT and reclaim its generation's arena.
    ///
    /// Requires a [`HeapDrained`] because live `RecordPayload`s and
    /// `TuplePayload`s hold raw pointers into that arena (hazard H15); only
    /// [`Runtime::teardown`](praxis_runtime::Runtime::teardown) can mint one.
    /// A `Jit` that is merely dropped leaks its arena, which is the pre-S8
    /// behaviour.
    pub fn retire(self, proof: HeapDrained) {
        let Jit {
            module,
            fn_ctx,
            generation,
        } = self;
        // The code goes first: it is what embedded the arena's addresses.
        drop(module);
        drop(fn_ctx);
        Generation::retire(generation, proof);
    }

    /// Reject a target the lowering does not actually support.
    ///
    /// The backend carries every value — `GcRef`s and all scalar payloads — as
    /// `I64` (`const GC`, ADR-037), reads `#[repr(C)]` runtime structs at
    /// offsets computed on the host, and bit-casts floats with an explicitly
    /// little-endian `MemFlags`. Each of those is a *host* assumption written
    /// as a constant rather than derived from the ISA, so a target with a
    /// different pointer width or endianness would miscompile silently. Fail
    /// loudly at construction instead.
    pub(crate) fn check_target(
        pointer: cranelift::codegen::ir::Type,
        endianness: cranelift::codegen::ir::Endianness,
    ) -> Result<(), JitError> {
        if pointer != cranelift::prelude::types::I64 {
            return Err(JitError::UnsupportedTarget(format!(
                "pointer type is {pointer}, but the lowering carries every value as i64"
            )));
        }
        if endianness != cranelift::codegen::ir::Endianness::Little {
            return Err(JitError::UnsupportedTarget(
                "big-endian targets are unsupported: float bit-casts use little-endian MemFlags"
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Compile a set of MIR functions, returning a map from function name to
    /// its declared [`FuncId`]. Each generated function has the signature
    /// `fn(RuntimeContext*, GcRef...) -> GcRef` with one `GcRef` param per MIR
    /// parameter.
    ///
    /// # Errors
    /// Returns a [`JitError`] if declaration, lowering, or finalization fails.
    pub fn compile(
        &mut self,
        funcs: &[MirFunction],
        db: &mut praxis_types::TypeDb,
    ) -> Result<HashMap<String, FuncId>, JitError> {
        // First pass: declare every function so they can reference each other
        // (and themselves, for recursion) before any is defined.
        let mut ids = HashMap::new();
        for f in funcs {
            let sig = self.signature_for(f);
            let id = self
                .module
                .declare_function(&f.name, Linkage::Export, &sig)
                .map_err(Box::new)?;
            ids.insert(f.name.clone(), id);
        }

        // Second pass: lower and define each function. Every piece of metadata
        // the lowering mints — schemas, names, debug locals, text literals —
        // goes into this JIT's generation rather than into a `Box::leak`.
        for f in funcs {
            lower::lower_function(
                &mut self.module,
                &mut self.fn_ctx,
                f,
                &ids,
                db,
                &self.generation,
            )
            .map_err(|e| JitError::Cranelift(e.into()))?;
        }

        // Finalize: allocate executable memory and resolve relocations.
        self.module.finalize_definitions().map_err(|e| {
            JitError::Cranelift(anyhow!("finalize_definitions failed: {e:?}").into())
        })?;
        Ok(ids)
    }

    /// Look up a finalized function's entry pointer by its [`FuncId`].
    ///
    /// # Safety
    /// The returned pointer must only be called with a valid `RuntimeContext*`
    /// and the correct number of `GcRef` arguments matching the function's MIR
    /// params. The `Jit` must outlive any call.
    pub unsafe fn entry(&self, id: FuncId) -> *const u8 {
        self.module.get_finalized_function(id)
    }

    /// The signature for one function: `fn(ctx: ptr, args: GcRef...) -> GcRef`.
    fn signature_for(&self, f: &MirFunction) -> Signature {
        let mut sig = Signature::new(CallConv::Fast);
        // Hidden first param: *mut RuntimeContext (raw pointer = i64).
        sig.params
            .push(AbiParam::new(cranelift::prelude::types::I64));
        for _ in &f.params {
            sig.params
                .push(AbiParam::new(cranelift::prelude::types::I64));
        }
        sig.returns
            .push(AbiParam::new(cranelift::prelude::types::I64));
        sig
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift::codegen::ir::{types, Endianness};

    /// The host is the only target the lowering is written for, and it must
    /// pass its own check.
    #[test]
    fn the_host_target_is_accepted() {
        Jit::new().expect("the host target must be accepted");
        Jit::check_target(types::I64, Endianness::Little).expect("i64 little-endian is the target");
    }

    /// A 32-bit pointer target must be rejected: the lowering carries every
    /// value, `GcRef`s included, as `i64`.
    #[test]
    fn a_non_i64_pointer_target_is_rejected() {
        let err = Jit::check_target(types::I32, Endianness::Little)
            .expect_err("a 32-bit pointer target must be rejected");
        assert!(
            matches!(err, JitError::UnsupportedTarget(ref m) if m.contains("i64")),
            "unexpected error: {err}"
        );
    }

    /// A big-endian target must be rejected: the float bit-cast pins
    /// little-endian `MemFlags` (ADR-037), and the `#[repr(C)]` runtime structs
    /// are read at host-computed offsets.
    #[test]
    fn a_big_endian_target_is_rejected() {
        let err = Jit::check_target(types::I64, Endianness::Big)
            .expect_err("a big-endian target must be rejected");
        assert!(
            matches!(err, JitError::UnsupportedTarget(ref m) if m.contains("endian")),
            "unexpected error: {err}"
        );
    }

    /// Every runtime symbol the JIT may import must be in the one symbol table.
    /// Registration used to be a second, hand-maintained list that had already
    /// drifted; `dlsym` hid the drift by finding the statically linked runtime.
    #[test]
    fn an_unknown_runtime_symbol_is_not_resolvable() {
        assert!(symbols::resolve("praxis_alloc_int").is_some());
        assert!(symbols::resolve("praxis_not_a_real_runtime_symbol").is_none());
    }
}
