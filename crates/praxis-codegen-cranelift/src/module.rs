//! The JIT module: owns the Cranelift `JITModule`, declares/defines functions,
//! and hands back callable entry pointers (§10, §10.5).
//!
//! One [`Jit`] owns one generation of compiled code (§10.5). M4 uses a single
//! generation per `run`. The `praxis_*` runtime symbols are registered through
//! `JITBuilder::symbol` so the JIT resolves imported calls without a linker.

use std::collections::HashMap;

use anyhow::anyhow;
use cranelift::codegen::isa::CallConv;
use cranelift::prelude::{AbiParam, Signature};
use cranelift_frontend::FunctionBuilderContext;
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId, Linkage, Module};
use praxis_mir::Function as MirFunction;
use praxis_runtime::{GcRef, RuntimeContext};

use crate::lower;
use crate::symbols;

/// A compiled, callable Praxis function: `fn(*mut RuntimeContext, ...) -> GcRef`.
/// (M4 functions take exactly one `GcRef` slot per param; this signature covers
/// the zero-extra-param entry case used for `main`.)
pub type RunnableFunction = unsafe extern "C" fn(*mut RuntimeContext, GcRef) -> GcRef;

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

/// Owns one JIT generation: the `JITModule` (code + data memory) and the
/// per-thread `FunctionBuilderContext` reused across lowered functions.
pub struct Jit {
    module: JITModule,
    fn_ctx: FunctionBuilderContext,
}

impl Jit {
    /// Create a fresh JIT with the `praxis_*` symbols registered.
    ///
    /// # Errors
    /// Returns [`JitError::UnsupportedTarget`] if the host CPU isn't supported.
    pub fn new() -> Result<Self, JitError> {
        let mut builder = JITBuilder::new(cranelift_module::default_libcall_names())
            .map_err(|e| JitError::UnsupportedTarget(format!("{e:?}")))?;
        // Register every `praxis_*` symbol the lowered code may call.
        let sym_names: &[&str] = &[
            "praxis_alloc_int",
            "praxis_alloc_bool",
            "praxis_alloc_unit",
            "praxis_alloc_text",
            "praxis_alloc_char",
            "praxis_int_load",
            "praxis_bool_load",
            "praxis_char_load",
            "praxis_int_add",
            "praxis_int_sub",
            "praxis_int_mul",
            "praxis_int_div",
            "praxis_int_rem",
            "praxis_int_neg",
            "praxis_int_eq",
            "praxis_int_ne",
            "praxis_int_lt",
            "praxis_int_gt",
            "praxis_int_le",
            "praxis_int_ge",
            "praxis_check_fault",
            "praxis_push_shadow_frame",
            "praxis_pop_shadow_frame",
            "praxis_vec_new",
            "praxis_vec_push",
            "praxis_vec_len",
            "praxis_vec_get",
            "praxis_vec_is_empty",
            "praxis_text_len",
            "praxis_text_is_empty",
            "praxis_text_get",
            "praxis_write_stdout",
            "praxis_get_input",
            "praxis_run_parser",
            "praxis_alloc_record",
            "praxis_record_set_field",
            "praxis_record_field",
            "praxis_alloc_enum",
            "praxis_enum_set_payload",
            "praxis_enum_tag",
            "praxis_enum_payload",
            "praxis_alloc_tuple",
            "praxis_tuple_set",
            "praxis_tuple_get",
            "praxis_struct_eq",
            "praxis_alloc_closure",
            "praxis_closure_set_capture",
            "praxis_closure_fn_ptr",
            "praxis_closure_capture",
            "praxis_push_debug_frame",
            "praxis_pop_debug_frame",
        ];
        for name in sym_names {
            if let Some(ptr) = symbols::resolve(name) {
                builder.symbol((*name).to_string(), ptr);
            }
        }
        let module = JITModule::new(builder);
        Ok(Jit {
            module,
            fn_ctx: FunctionBuilderContext::new(),
        })
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
        db: &praxis_types::TypeDb,
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

        // Second pass: lower and define each function.
        for f in funcs {
            lower::lower_function(&mut self.module, &mut self.fn_ctx, f, &ids, db)
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
