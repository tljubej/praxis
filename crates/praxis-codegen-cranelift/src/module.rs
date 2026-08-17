//! The JIT module: owns the Cranelift `JITModule`, declares/defines functions,
//! and hands back callable entry pointers (§10, §10.5).
//!
//! One [`Jit`] owns one generation of compiled code (§10.5), and a `run` uses a
//! single generation. The `praxis_*` runtime symbols are registered through
//! `JITBuilder::symbol` so the JIT resolves imported calls without a linker.

use std::collections::HashMap;
use std::rc::Rc;

use anyhow::anyhow;
use cranelift::codegen::isa::CallConv;
use cranelift::prelude::{AbiParam, Signature};
use cranelift_frontend::FunctionBuilderContext;
use cranelift_jit::{ArenaMemoryProvider, JITBuilder, JITModule};
use cranelift_module::{FuncId, Linkage, Module};
use praxis_mir::Function as MirFunction;
use praxis_runtime::{GcRef, HeapDrained, RuntimeContext};

use crate::generation::Generation;
use crate::lower;
use crate::symbols;

/// A compiled, callable entry point: `fn(*mut RuntimeContext) -> GcRef`.
///
/// This is exactly what `lower::abi_signature` emits for a zero-parameter
/// function such as `main`: the hidden context pointer and nothing else. A
/// function *with* parameters is called through its own transmuted type (see
/// the debugger's `call_with_arity`).
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
    #[error(
        "could not reserve {CODE_RESERVATION_BYTES} bytes of address space for generated code: {0}"
    )]
    CodeReservation(String),
}

/// Owns one JIT generation: the `JITModule` (code + data memory), the
/// per-thread `FunctionBuilderContext` reused across lowered functions, and the
/// [`Generation`] arena that owns every piece of metadata the generated code
/// and the runtime reach by raw pointer.
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

/// The Cranelift settings every Praxis JIT is built with, written out rather
/// than left to `JITBuilder::new`'s empty list, so the decision is visible at
/// the point it takes effect.
///
/// **`opt_level` is `"speed"`.** Measured with `benchmarks/ab.py`, arms
/// differing in this constant alone, two independent passes: `collatz` +16.5% ±
/// 0.8%, `tree` +4.2% ± 0.3%, `primes` −1.0%, `bfs` −1.1%; suite geometric mean
/// **1.025×**, the remaining rows inside the 2% floor. `primes` and `bfs` are a
/// real if unresolved *cost* and are recorded rather than netted away. Compile
/// time is +0.1 ms on a 6.9 ms floor.
///
/// **The win is not in the loop body**, so a per-iteration instruction count is
/// the wrong instrument for re-measuring it: with `"speed"` the hot cycle is
/// *two instructions longer* (`collatz` 123→125). What moves is the whole
/// function — `collatz` goes 805→761 instructions, **3460→3208 bytes (−7%)**,
/// and 38→34 cold blocks. The mid-end is cleaning up the out-of-line paths
/// ADR-117's fold and ADR-119's three bail-outs put there, an I-cache and layout
/// effect a loop-body count structurally cannot see.
///
/// The corollary is a standing constraint on the lowering: toggling this flag
/// leaves a hot loop's instructions the same ones, so what the lowering emits
/// there is not redundant to Cranelift, and redundancy inside a loop is the
/// lowering's to remove rather than the optimizer's.
///
/// Cranelift's settings builder is stringly typed, so a name or a value it does
/// not know is an error from `JITBuilder::with_flags` at run time and not a build
/// failure. `the_opt_level_flag_is_accepted_and_takes_effect` is the build
/// failure.
pub(crate) const CRANELIFT_FLAGS: &[(&str, &str)] = &[("opt_level", "speed")];

/// The contiguous address space one [`Jit`] reserves up front to carve every
/// piece of generated code out of.
///
/// **Every generated function has to land within ±2GiB of every other one, and
/// this reservation is what makes that true.** [`Jit::compile`] declares user
/// functions `Linkage::Export`; that linkage is *final*, so Cranelift treats
/// each cross-function reference as colocated and picks its ±2GiB encodings —
/// `call rel32` for a call, `lea (%rip)` for the `func_addr` behind a closure.
/// Both are `Reloc::X86CallPCRel4`, and cranelift-jit resolves them with a bare
/// `i32::try_from(..).unwrap()`. An out-of-range target is therefore a **panic
/// inside the relocation pass**, not a `ModuleError` this crate could turn into
/// a [`JitError`], so the range cannot be recovered from — only established.
///
/// Cranelift's default `SystemMemoryProvider` does not establish it. It takes a
/// fresh `mmap` per code chunk and never over-allocates, so a ~3KB function gets
/// roughly a chunk to itself and the spacing between chunks is whatever the
/// kernel felt like. It is adjacent often enough to look correct and is not a
/// guarantee. `ArenaMemoryProvider` hands out every chunk from one reservation
/// instead, which is what makes the encoding Cranelift already chose legal.
///
/// **The size is a ceiling on one `Jit`'s total generated code**, and exhausting
/// it fails compilation rather than miscompiling: the `ModuleError::Allocation`
/// leaves `define_function` and arrives as a [`JitError::Cranelift`]. No program
/// in `tests/aoc-corpus` comes close — the largest emits 58,508 bytes and the
/// widest is 17 functions, so this leaves ~1100× headroom (ADR-153).
///
/// It costs address space and not memory — the region is reserved `PROT_NONE`
/// and pages are committed as segments are handed out, so the resident cost
/// stays the size of the code. That matters because a finalized reservation is
/// deliberately leaked (generated code stays callable), and the debugger mints a
/// `Jit` per `p EXPR`: a long session leaks this constant per expression in
/// address space, which is affordable exactly because it is not memory.
///
/// A target whose relocations carry their own out-of-range fallback does not
/// need any of this — aarch64 rewrites a too-far `Reloc::Arm64Call` into a
/// veneer. That is why the bound must be held here rather than trusted to hold
/// itself: it is invisible on a host that repairs it.
pub(crate) const CODE_RESERVATION_BYTES: usize = 64 << 20;

impl Jit {
    /// Create a fresh JIT with the `praxis_*` symbols registered, in its own
    /// new generation.
    ///
    /// # Errors
    /// Returns [`JitError::UnsupportedTarget`] if the host CPU isn't supported,
    /// or if its pointer width or endianness is not the one the lowering
    /// assumes (see [`Jit::check_target`]); [`JitError::CodeReservation`] if the
    /// address space for generated code cannot be reserved (see
    /// [`CODE_RESERVATION_BYTES`]).
    pub fn new() -> Result<Self, JitError> {
        Self::in_generation(Rc::new(Generation::new()))
    }

    /// Create a fresh JIT that compiles into an *existing* generation.
    ///
    /// The debugger uses this: every `p EXPR` compiles a throwaway module, but
    /// they all share the session's one arena, so the metadata they mint is
    /// interned instead of accumulating. The values a `p` leaves in the heap
    /// keep pointing at schemas the shared generation still owns.
    ///
    /// # Errors
    /// As [`Jit::new`].
    pub fn in_generation(generation: Rc<Generation>) -> Result<Self, JitError> {
        let mut builder =
            JITBuilder::with_flags(CRANELIFT_FLAGS, cranelift_module::default_libcall_names())
                .map_err(|e| JitError::UnsupportedTarget(format!("{e:?}")))?;
        // Resolve `praxis_*` imports through `symbols::resolve` — the one
        // table. A second registration list here would drift silently: the JIT
        // falls back to `dlsym`, which finds the statically linked runtime and
        // hides any omission.
        builder.symbol_lookup_fn(Box::new(symbols::resolve));
        // Every chunk out of one reservation, so the colocated ±2GiB encodings
        // Cranelift picks for cross-function references are in range by
        // construction rather than by the kernel's habit of placing consecutive
        // `mmap`s adjacently. See [`CODE_RESERVATION_BYTES`].
        builder.memory_provider(Box::new(
            ArenaMemoryProvider::new_with_size(CODE_RESERVATION_BYTES)
                .map_err(|e| JitError::CodeReservation(e.to_string()))?,
        ));
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
    /// `TuplePayload`s hold raw pointers into that arena (ADR-043); only
    /// [`Runtime::teardown`](praxis_runtime::Runtime::teardown) can mint one.
    /// A `Jit` that is merely dropped leaks its arena.
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

    /// Every runtime symbol the JIT may import must be in the one symbol table,
    /// and nothing else must resolve through it.
    #[test]
    fn an_unknown_runtime_symbol_is_not_resolvable() {
        assert!(symbols::resolve("praxis_alloc_int").is_some());
        assert!(symbols::resolve("praxis_not_a_real_runtime_symbol").is_none());
    }

    /// The optimization level is set explicitly, and it reached the ISA.
    ///
    /// Both halves are the test. A settings name or value Cranelift does not
    /// know is an `Err` from `with_flags` — see the test below — so "it
    /// compiles" says nothing about whether [`CRANELIFT_FLAGS`] is spelled
    /// right, and `Jit::new()` succeeding is what says the pair was accepted.
    /// Reading the level back off the ISA is what says it governs the
    /// compilations this `Jit` performs, rather than having been accepted and
    /// dropped.
    #[test]
    fn the_opt_level_flag_is_accepted_and_takes_effect() {
        let jit = Jit::new().expect("the explicit flags must be accepted");
        assert_eq!(
            jit.module.isa().flags().opt_level(),
            cranelift::codegen::settings::OptLevel::Speed,
            "the fifth measurement reopened this: `collatz` +16.5%, `tree` \
             +4.2%, both reproduced, and the mechanism is code size rather \
             than the loop body"
        );
    }

    /// A settings pair Cranelift does not know is a run-time error, which is
    /// the reason the test above has to read the level back rather than trust
    /// the constant.
    #[test]
    fn a_flag_value_cranelift_does_not_know_is_rejected_at_run_time() {
        let err = JITBuilder::with_flags(
            &[("opt_level", "fastest")],
            cranelift_module::default_libcall_names(),
        )
        .err()
        .expect("`fastest` is not one of Cranelift's three optimization levels");
        assert!(
            format!("{err}").contains("none, speed, speed_and_size"),
            "and the three legal values are the ones `CRANELIFT_FLAGS` must be \
             spelled from: {err}"
        );
    }
}
