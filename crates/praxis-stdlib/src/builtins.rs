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
        .entry(map_insert())
        .entry(map_get())
        .entry(map_contains())
        .entry(map_remove())
        .entry(map_len())
        .entry(map_is_empty())
        .entry(set_insert())
        .entry(set_remove())
        .entry(set_contains())
        .entry(set_len())
        .entry(set_is_empty())
        .entry(counter_get())
        .entry(counter_inc())
        .entry(counter_len())
        .entry(counter_is_empty())
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

// --- Map / Set / Counter methods (M8-WS3, §6.1, §11.3) -------------------

/// The `Map[K, V]` receiver pattern: two type args (key, value).
fn map_of_k_v() -> TypePattern {
    TypePattern::Collection {
        ctor: CollectionCtor::Map,
        args: vec![TypePattern::Var("K"), TypePattern::Var("V")],
    }
}

/// The `Set[T]` receiver pattern.
fn set_of_t() -> TypePattern {
    TypePattern::Collection {
        ctor: CollectionCtor::Set,
        args: vec![TypePattern::Var("T")],
    }
}

/// The `Counter[T]` receiver pattern (key type only; values are Int).
fn counter_of_t() -> TypePattern {
    TypePattern::Collection {
        ctor: CollectionCtor::Counter,
        args: vec![TypePattern::Var("T")],
    }
}

fn map_insert() -> MethodEntry {
    MethodEntry {
        receiver: map_of_k_v(),
        name: "insert",
        params: vec![TypePattern::Var("K"), TypePattern::Var("V")],
        result: TypePattern::Unit,
        purity: Purity::Impure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::RuntimeSymbol("praxis_map_insert"),
        doc: "Set `key` to `value`, replacing any prior value; returns Unit.",
        stability: Stability::Stable,
    }
}

fn map_get() -> MethodEntry {
    MethodEntry {
        receiver: map_of_k_v(),
        name: "get",
        params: vec![TypePattern::Var("K")],
        // For now the result is V (Unit if absent); a real Option[V] is a
        // follow-up once Option lands more broadly.
        result: TypePattern::Var("V"),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_map_get"),
        doc: "The value for `key`, or Unit if absent (use `contains` to distinguish).",
        stability: Stability::Stable,
    }
}

fn map_contains() -> MethodEntry {
    MethodEntry {
        receiver: map_of_k_v(),
        name: "contains",
        params: vec![TypePattern::Var("K")],
        result: TypePattern::Scalar(ScalarType::Bool),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_map_contains"),
        doc: "True iff `key` is present in the map.",
        stability: Stability::Stable,
    }
}

fn map_remove() -> MethodEntry {
    MethodEntry {
        receiver: map_of_k_v(),
        name: "remove",
        params: vec![TypePattern::Var("K")],
        result: TypePattern::Unit,
        purity: Purity::Impure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_map_remove"),
        doc: "Remove `key` if present; returns Unit.",
        stability: Stability::Stable,
    }
}

fn map_len() -> MethodEntry {
    MethodEntry {
        receiver: map_of_k_v(),
        name: "len",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_map_len"),
        doc: "Number of entries in the map.",
        stability: Stability::Stable,
    }
}

fn map_is_empty() -> MethodEntry {
    MethodEntry {
        receiver: map_of_k_v(),
        name: "is_empty",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Bool),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_map_is_empty"),
        doc: "True iff the map has no entries.",
        stability: Stability::Stable,
    }
}

fn set_insert() -> MethodEntry {
    MethodEntry {
        receiver: set_of_t(),
        name: "insert",
        params: vec![TypePattern::Var("T")],
        result: TypePattern::Unit,
        purity: Purity::Impure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::RuntimeSymbol("praxis_set_insert"),
        doc: "Add `value` to the set; returns Unit.",
        stability: Stability::Stable,
    }
}

fn set_remove() -> MethodEntry {
    MethodEntry {
        receiver: set_of_t(),
        name: "remove",
        params: vec![TypePattern::Var("T")],
        result: TypePattern::Unit,
        purity: Purity::Impure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_set_remove"),
        doc: "Remove `value` if present; returns Unit.",
        stability: Stability::Stable,
    }
}

fn set_contains() -> MethodEntry {
    MethodEntry {
        receiver: set_of_t(),
        name: "contains",
        params: vec![TypePattern::Var("T")],
        result: TypePattern::Scalar(ScalarType::Bool),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_set_contains"),
        doc: "True iff `value` is in the set.",
        stability: Stability::Stable,
    }
}

fn set_len() -> MethodEntry {
    MethodEntry {
        receiver: set_of_t(),
        name: "len",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_set_len"),
        doc: "Number of elements in the set.",
        stability: Stability::Stable,
    }
}

fn set_is_empty() -> MethodEntry {
    MethodEntry {
        receiver: set_of_t(),
        name: "is_empty",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Bool),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_set_is_empty"),
        doc: "True iff the set has no elements.",
        stability: Stability::Stable,
    }
}

fn counter_get() -> MethodEntry {
    MethodEntry {
        receiver: counter_of_t(),
        name: "get",
        params: vec![TypePattern::Var("T")],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::RuntimeSymbol("praxis_counter_get"),
        doc: "The count for `key`, or zero if absent (never faults).",
        stability: Stability::Stable,
    }
}

fn counter_inc() -> MethodEntry {
    MethodEntry {
        receiver: counter_of_t(),
        name: "inc",
        params: vec![TypePattern::Var("T")],
        result: TypePattern::Unit,
        purity: Purity::Impure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::RuntimeSymbol("praxis_counter_inc"),
        doc: "Increment the count for `key` by one; returns Unit.",
        stability: Stability::Stable,
    }
}

fn counter_len() -> MethodEntry {
    MethodEntry {
        receiver: counter_of_t(),
        name: "len",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_counter_len"),
        doc: "Number of distinct keys in the counter.",
        stability: Stability::Stable,
    }
}

fn counter_is_empty() -> MethodEntry {
    MethodEntry {
        receiver: counter_of_t(),
        name: "is_empty",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Bool),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_counter_is_empty"),
        doc: "True iff the counter has no keys.",
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
