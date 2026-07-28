//! Resolution of the `praxis_*` runtime symbols the JIT'd code calls (§10.2).
//!
//! The Cranelift JIT resolves imported symbols by name through a resolver
//! closure. That closure is the last place in the pipeline where a symbol is
//! still a string, so this module is a thin adapter over the two typed halves:
//! [`RuntimeSymbol::from_name`] recovers the symbol, and
//! `praxis_runtime::abi::address` gives its address. Both are exhaustive over
//! the manifest in `praxis_stdlib::abi`, so there is no list here to drift.

use praxis_stdlib::abi::RuntimeSymbol;

/// Look up a `praxis_*` runtime symbol by name, returning its address.
///
/// Returns `None` for a name the manifest does not contain — Cranelift then
/// reports the unresolved import as a definition error, surfaced as a
/// [`crate::JitError`]. There is deliberately no `dlsym` fallback: it would
/// find any `#[no_mangle]` symbol of the statically linked runtime, so a symbol
/// the compiler never declared would "work" locally and the manifest would be
/// free to rot.
#[must_use]
pub fn resolve(name: &str) -> Option<*const u8> {
    RuntimeSymbol::from_name(name).map(praxis_runtime::abi::address)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every symbol in the manifest resolves to a real, distinct address. This
    /// is what makes "declared in the manifest" and "callable from JIT'd code"
    /// the same statement.
    #[test]
    fn every_manifest_symbol_resolves_to_a_distinct_address() {
        let mut seen = std::collections::HashMap::new();
        for &sym in RuntimeSymbol::ALL {
            let addr = resolve(sym.name())
                .unwrap_or_else(|| panic!("{sym} is in the manifest but does not resolve"));
            assert!(!addr.is_null(), "{sym} resolved to null");
            if let Some(other) = seen.insert(addr, sym) {
                panic!("{sym} and {other} share an address — a copy-paste in the address table");
            }
        }
    }

    #[test]
    fn an_unknown_name_does_not_resolve() {
        assert!(resolve("praxis_not_a_real_runtime_symbol").is_none());
        assert!(resolve("malloc").is_none());
    }
}
