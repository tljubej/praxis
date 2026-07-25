//! The built-in method catalog (§16.2): one structured table of every built-in
//! method, filled in incrementally per milestone.
//!
//! This is the **single source of truth** the type checker, HIR lowering, and
//! code generator consume (rule 20.3). [`builtin_catalog`] returns a finalized,
//! duplicate-free [`MethodCatalog`]; the builder rejects any duplicate
//! `(receiver, name, arity)` triple so an accidental overload is impossible
//! ("make illegal states unrepresentable").
//!
//! M5 populates the `Vec[T]` method surface (`push`, `len`, `get`,
//! `is_empty`). M8-WS2 adds `Deque[T]`. The rest of the collection set
//! (Map/Set/Counter/Heap) lands in later M8 workstreams; Text methods and
//! `out(...)` arrive with the features that exercise them.

use crate::catalog::MethodCatalog;
use crate::type_pattern::{CollectionCtor, ScalarType};
use crate::{MethodEntry, MethodLowering, Purity, Stability, TypePattern};

/// The `Vec[T]` receiver pattern, used by every Vec method entry.
fn vec_of_t() -> TypePattern {
    TypePattern::Collection {
        ctor: CollectionCtor::Vec,
        args: vec![TypePattern::Var("T")],
    }
}

/// Build the finalized built-in method catalog for this milestone.
///
/// # Panics
/// Panics if two entries share a `(receiver, name, arity)` triple — that is a
/// build-time catalog bug, never a user-facing condition.
#[must_use]
pub fn builtin_catalog() -> MethodCatalog {
    MethodCatalog::build()
        .entry(vec_push())
        .entry(vec_len())
        .entry(vec_get())
        .entry(vec_is_empty())
        .entry(deque_push_front())
        .entry(deque_push_back())
        .entry(deque_pop_front())
        .entry(deque_pop_back())
        .entry(deque_len())
        .entry(deque_get())
        .entry(deque_is_empty())
        .entry(text_len())
        .entry(text_is_empty())
        .entry(text_get())
        .finish()
        .expect("built-in catalog must be duplicate-free")
}

// --- Text methods --------------------------------------------------------

fn text_receiver() -> TypePattern {
    TypePattern::Scalar(ScalarType::Text)
}

fn text_len() -> MethodEntry {
    MethodEntry {
        receiver: text_receiver(),
        name: "len",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_text_len"),
        doc: "Number of Unicode scalar values (chars) in the text.",
        stability: Stability::Stable,
    }
}

fn text_is_empty() -> MethodEntry {
    MethodEntry {
        receiver: text_receiver(),
        name: "is_empty",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Bool),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_text_is_empty"),
        doc: "True iff the text has no chars.",
        stability: Stability::Stable,
    }
}

fn text_get() -> MethodEntry {
    MethodEntry {
        receiver: text_receiver(),
        name: "get",
        params: vec![TypePattern::Scalar(ScalarType::Int)],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        can_fault: true,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_text_get"),
        doc: "The scalar value of the char at `index`; faults if out of range.",
        stability: Stability::Stable,
    }
}

fn vec_push() -> MethodEntry {
    MethodEntry {
        receiver: vec_of_t(),
        name: "push",
        params: vec![TypePattern::Var("T")],
        result: TypePattern::Unit,
        purity: Purity::Impure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::RuntimeSymbol("praxis_vec_push"),
        doc: "Append a value to the end; returns Unit.",
        stability: Stability::Stable,
    }
}

fn vec_len() -> MethodEntry {
    MethodEntry {
        receiver: vec_of_t(),
        name: "len",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_vec_len"),
        doc: "Number of elements in the vector.",
        stability: Stability::Stable,
    }
}

fn vec_get() -> MethodEntry {
    MethodEntry {
        receiver: vec_of_t(),
        name: "get",
        params: vec![TypePattern::Scalar(ScalarType::Int)],
        result: TypePattern::Var("T"),
        purity: Purity::Pure,
        can_fault: true,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_vec_get"),
        doc: "The element at `index`; faults `IndexOutOfBounds` if out of range.",
        stability: Stability::Stable,
    }
}

fn vec_is_empty() -> MethodEntry {
    MethodEntry {
        receiver: vec_of_t(),
        name: "is_empty",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Bool),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_vec_is_empty"),
        doc: "True iff the vector has no elements.",
        stability: Stability::Stable,
    }
}

// --- Deque methods (M8-WS2, §6.1) ----------------------------------------

/// The `Deque[T]` receiver pattern, used by every Deque method entry.
fn deque_of_t() -> TypePattern {
    TypePattern::Collection {
        ctor: CollectionCtor::Deque,
        args: vec![TypePattern::Var("T")],
    }
}

fn deque_push_front() -> MethodEntry {
    MethodEntry {
        receiver: deque_of_t(),
        name: "push_front",
        params: vec![TypePattern::Var("T")],
        result: TypePattern::Unit,
        purity: Purity::Impure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::RuntimeSymbol("praxis_deque_push_front"),
        doc: "Prepend a value to the front; returns Unit.",
        stability: Stability::Stable,
    }
}

fn deque_push_back() -> MethodEntry {
    MethodEntry {
        receiver: deque_of_t(),
        name: "push_back",
        params: vec![TypePattern::Var("T")],
        result: TypePattern::Unit,
        purity: Purity::Impure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::RuntimeSymbol("praxis_deque_push_back"),
        doc: "Append a value to the back; returns Unit.",
        stability: Stability::Stable,
    }
}

fn deque_pop_front() -> MethodEntry {
    MethodEntry {
        receiver: deque_of_t(),
        name: "pop_front",
        params: vec![],
        result: TypePattern::Var("T"),
        purity: Purity::Impure,
        can_fault: true,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_deque_pop_front"),
        doc: "Remove and return the front element; faults if empty.",
        stability: Stability::Stable,
    }
}

fn deque_pop_back() -> MethodEntry {
    MethodEntry {
        receiver: deque_of_t(),
        name: "pop_back",
        params: vec![],
        result: TypePattern::Var("T"),
        purity: Purity::Impure,
        can_fault: true,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_deque_pop_back"),
        doc: "Remove and return the back element; faults if empty.",
        stability: Stability::Stable,
    }
}

fn deque_len() -> MethodEntry {
    MethodEntry {
        receiver: deque_of_t(),
        name: "len",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_deque_len"),
        doc: "Number of elements in the deque.",
        stability: Stability::Stable,
    }
}

fn deque_get() -> MethodEntry {
    MethodEntry {
        receiver: deque_of_t(),
        name: "get",
        params: vec![TypePattern::Scalar(ScalarType::Int)],
        result: TypePattern::Var("T"),
        purity: Purity::Pure,
        can_fault: true,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_deque_get"),
        doc: "The element at `index` (0-based from the front); faults if out of range.",
        stability: Stability::Stable,
    }
}

fn deque_is_empty() -> MethodEntry {
    MethodEntry {
        receiver: deque_of_t(),
        name: "is_empty",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Bool),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_deque_is_empty"),
        doc: "True iff the deque has no elements.",
        stability: Stability::Stable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_catalog_has_vec_methods() {
        let cat = builtin_catalog();
        assert!(cat.len() >= 4);
        let vec_pat = vec_of_t();
        let push_hits: Vec<_> = cat.by_receiver_and_name(&vec_pat, "push").collect();
        assert_eq!(push_hits.len(), 1);
        assert_eq!(
            push_hits[0].lowering,
            MethodLowering::RuntimeSymbol("praxis_vec_push")
        );
    }

    #[test]
    fn builtin_catalog_get_can_fault() {
        let cat = builtin_catalog();
        let vec_pat = vec_of_t();
        let get = cat
            .by_receiver_and_name(&vec_pat, "get")
            .next()
            .expect("vec.get exists");
        assert!(get.can_fault);
    }
}
