//! Registration of the `praxis_*` runtime symbols the JIT'd code calls (§10.2).
//!
//! The Cranelift JIT resolves imported symbols by name through a resolver
//! closure; this module hands it `(name -> function pointer)` pairs for every
//! `praxis_*` extern wrapper in `praxis-runtime::abi`. Each entry is looked up
//! by the exact symbol name the lowering emits.

use praxis_runtime::abi::*;
use praxis_runtime::shadow_frame::{praxis_pop_shadow_frame, praxis_push_shadow_frame};

/// Look up a `praxis_*` runtime symbol by name, returning its address as a
/// `*const u8`. Returns `None` for unknown names (Cranelift then reports the
/// unresolved import as a definition error — surfaced as a [`crate::JitError`]).
#[must_use]
pub fn resolve(name: &str) -> Option<*const u8> {
    let ptr: *const () = match name {
        "praxis_alloc_int" => praxis_alloc_int as *const (),
        "praxis_alloc_bool" => praxis_alloc_bool as *const (),
        "praxis_alloc_unit" => praxis_alloc_unit as *const (),
        "praxis_alloc_text" => praxis_alloc_text as *const (),
        "praxis_alloc_char" => praxis_alloc_char as *const (),
        "praxis_int_load" => praxis_int_load as *const (),
        "praxis_bool_load" => praxis_bool_load as *const (),
        "praxis_char_load" => praxis_char_load as *const (),
        "praxis_int_add" => praxis_int_add as *const (),
        "praxis_int_sub" => praxis_int_sub as *const (),
        "praxis_int_mul" => praxis_int_mul as *const (),
        "praxis_int_div" => praxis_int_div as *const (),
        "praxis_int_rem" => praxis_int_rem as *const (),
        "praxis_int_neg" => praxis_int_neg as *const (),
        "praxis_int_eq" => praxis_int_eq as *const (),
        "praxis_int_ne" => praxis_int_ne as *const (),
        "praxis_int_lt" => praxis_int_lt as *const (),
        "praxis_int_gt" => praxis_int_gt as *const (),
        "praxis_int_le" => praxis_int_le as *const (),
        "praxis_int_ge" => praxis_int_ge as *const (),
        "praxis_check_fault" => praxis_check_fault as *const (),
        "praxis_push_shadow_frame" => praxis_push_shadow_frame as *const (),
        "praxis_pop_shadow_frame" => praxis_pop_shadow_frame as *const (),
        "praxis_vec_new" => praxis_vec_new as *const (),
        "praxis_vec_push" => praxis_vec_push as *const (),
        "praxis_vec_len" => praxis_vec_len as *const (),
        "praxis_vec_get" => praxis_vec_get as *const (),
        "praxis_vec_is_empty" => praxis_vec_is_empty as *const (),
        "praxis_text_len" => praxis_text_len as *const (),
        "praxis_text_is_empty" => praxis_text_is_empty as *const (),
        "praxis_text_get" => praxis_text_get as *const (),
        "praxis_write_stdout" => praxis_write_stdout as *const (),
        "praxis_get_input" => praxis_get_input as *const (),
        "praxis_run_parser" => praxis_run_parser as *const (),
        "praxis_push_debug_frame" => praxis_runtime::debug::praxis_push_debug_frame as *const (),
        "praxis_pop_debug_frame" => praxis_runtime::debug::praxis_pop_debug_frame as *const (),
        _ => return None,
    };
    Some(ptr as *const u8)
}
