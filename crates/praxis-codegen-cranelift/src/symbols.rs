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

    /// Wrappers whose compiled bodies are **byte-identical**, and which an
    /// optimized build is therefore allowed to collapse onto one address.
    ///
    /// rustc defaults to `-Zmerge-functions=aliases`, so at any `opt-level > 0`
    /// LLVM emits one body for a set of identical functions and makes the other
    /// `#[no_mangle]` names aliases of it. Rust has never promised that two
    /// functions have two addresses, and a release build of the `praxis_*`
    /// wrappers is where that shows. Nothing is broken by it — merging happens
    /// *because* the instructions already agree, so every alias computes the
    /// answer its own name promises.
    ///
    /// Listing the sets is what keeps the copy-paste guard below meaningful: an
    /// address shared by two wrappers that are *not* listed here is a table
    /// entry pointing at the wrong function, because two different bodies never
    /// merge. Merging is permitted, never required — a build that folds none of
    /// these still passes.
    const MAY_SHARE_AN_ADDRESS: &[&[RuntimeSymbol]] = &[
        // `is_empty` over a `VecDeque`, a `HashSet` and a `Counter`'s `HashMap`:
        // each reads a length at the same payload offset and selects the boxed
        // `True` or `False`. Nine instructions, and the same nine.
        &[
            RuntimeSymbol::CounterIsEmpty,
            RuntimeSymbol::DequeIsEmpty,
            RuntimeSymbol::SetIsEmpty,
        ],
        // The two heaps differ only in the `Reverse` wrapper on their entries,
        // which is a comparison detail and absent from both of these.
        &[RuntimeSymbol::MaxHeapIsEmpty, RuntimeSymbol::MinHeapIsEmpty],
        &[RuntimeSymbol::MaxHeapPeek, RuntimeSymbol::MinHeapPeek],
        // A closure's captures and a tuple's elements are the same slot array
        // under two names, so a bounds-checked read and write of slot `i` are
        // one body each.
        &[RuntimeSymbol::ClosureCapture, RuntimeSymbol::TupleGet],
        &[RuntimeSymbol::ClosureSetCapture, RuntimeSymbol::TupleSet],
        // Both load one word from the front of the payload and return it raw:
        // a closure's function pointer, a `Float`'s bits.
        &[RuntimeSymbol::ClosureFnPtr, RuntimeSymbol::FloatLoad],
    ];

    /// Whether `a` and `b` are known to compile to the same instructions.
    fn bodies_are_known_identical(a: RuntimeSymbol, b: RuntimeSymbol) -> bool {
        MAY_SHARE_AN_ADDRESS
            .iter()
            .any(|set| set.contains(&a) && set.contains(&b))
    }

    /// Every symbol in the manifest resolves to a real address, and no two
    /// unrelated symbols resolve to the same one. This is what makes "declared
    /// in the manifest" and "callable from JIT'd code" the same statement.
    #[test]
    fn every_manifest_symbol_resolves_to_a_distinct_address() {
        let mut seen = std::collections::HashMap::new();
        for &sym in RuntimeSymbol::ALL {
            let addr = resolve(sym.name())
                .unwrap_or_else(|| panic!("{sym} is in the manifest but does not resolve"));
            assert!(!addr.is_null(), "{sym} resolved to null");
            if let Some(other) = seen.insert(addr, sym) {
                assert!(
                    bodies_are_known_identical(sym, other),
                    "{sym} and {other} share an address. Either the address table \
                     maps one of them to the other's function — a copy-paste — or \
                     their bodies have become identical and the optimizer merged \
                     them. Read both before deciding; only the second one belongs \
                     in MAY_SHARE_AN_ADDRESS."
                );
            }
        }
    }

    #[test]
    fn an_unknown_name_does_not_resolve() {
        assert!(resolve("praxis_not_a_real_runtime_symbol").is_none());
        assert!(resolve("malloc").is_none());
    }
}
