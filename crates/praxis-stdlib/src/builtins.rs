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
        .entry(max_heap_push())
        .entry(max_heap_pop())
        .entry(max_heap_peek())
        .entry(max_heap_len())
        .entry(max_heap_is_empty())
        .entry(min_heap_push())
        .entry(min_heap_pop())
        .entry(min_heap_peek())
        .entry(min_heap_len())
        .entry(min_heap_is_empty())
        .entry(bitset_insert())
        .entry(bitset_remove())
        .entry(bitset_contains())
        .entry(bitset_len())
        .entry(bitset_is_empty())
        .entry(grid_width())
        .entry(grid_height())
        .entry(grid_get())
        .entry(grid_set())
        .entry(grid_contains())
        .entry(grid_neighbors4())
        .entry(grid_neighbors8())
        .entry(grid_positions())
        .entry(grid_cells())
        .entry(grid_row())
        .entry(grid_column())
        .entry(grid_find())
        .entry(grid_find_all())
        .entry(grid_transpose())
        .entry(grid_rotate_left())
        .entry(grid_rotate_right())
        // Pipeline combinators (M8-WS8, §6.3). These are intrinsics lowered by
        // the compiler into fused loops; they accept a closure and return either
        // a Seq[T] (streaming) or a scalar/Vec (sink). Defined on Vec[T] and
        // Seq[T] receivers so `v.map(f)` works directly and chains fuse.
        .entry(seq_map_on_vec())
        .entry(seq_map_on_seq())
        .entry(seq_filter_on_vec())
        .entry(seq_filter_on_seq())
        .entry(seq_fold_on_vec())
        .entry(seq_fold_on_seq())
        .entry(seq_sum_on_vec())
        .entry(seq_sum_on_seq())
        .entry(seq_count_on_vec())
        .entry(seq_count_on_seq())
        .entry(seq_collect_on_vec())
        .entry(seq_collect_on_seq())
        // M8-WS11: the remaining non-barrier combinators. Each is an intrinsic
        // fused by the MIR pipeline recognizer. Barriers (sorted/unique/
        // frequencies/chunks/windows) are intentionally absent — they need new
        // runtime helpers (separate workstream).
        .entry(seq_take_on_vec())
        .entry(seq_take_on_seq())
        .entry(seq_skip_on_vec())
        .entry(seq_skip_on_seq())
        .entry(seq_take_while_on_vec())
        .entry(seq_take_while_on_seq())
        .entry(seq_enumerate_on_vec())
        .entry(seq_enumerate_on_seq())
        .entry(seq_zip_on_vec())
        .entry(seq_zip_on_seq())
        .entry(seq_flat_map_on_vec())
        .entry(seq_flat_map_on_seq())
        .entry(seq_filter_map_on_vec())
        .entry(seq_filter_map_on_seq())
        .entry(seq_product_on_vec())
        .entry(seq_product_on_seq())
        .entry(seq_min_on_vec())
        .entry(seq_min_on_seq())
        .entry(seq_max_on_vec())
        .entry(seq_max_on_seq())
        .entry(seq_min_by_on_vec())
        .entry(seq_min_by_on_seq())
        .entry(seq_max_by_on_vec())
        .entry(seq_max_by_on_seq())
        .entry(seq_any_on_vec())
        .entry(seq_any_on_seq())
        .entry(seq_all_on_vec())
        .entry(seq_all_on_seq())
        .entry(seq_find_on_vec())
        .entry(seq_find_on_seq())
        .entry(seq_position_on_vec())
        .entry(seq_position_on_seq())
        .entry(seq_reduce_on_vec())
        .entry(seq_reduce_on_seq())
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

// --- MinHeap[T] / MaxHeap[T] methods (M8-WS4, §6.1) ---------------------

fn min_heap_of_t() -> TypePattern {
    TypePattern::Collection {
        ctor: CollectionCtor::MinHeap,
        args: vec![TypePattern::Var("T")],
    }
}

fn max_heap_of_t() -> TypePattern {
    TypePattern::Collection {
        ctor: CollectionCtor::MaxHeap,
        args: vec![TypePattern::Var("T")],
    }
}

fn max_heap_push() -> MethodEntry {
    MethodEntry {
        receiver: max_heap_of_t(),
        name: "push",
        params: vec![TypePattern::Var("T")],
        result: TypePattern::Unit,
        purity: Purity::Impure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::RuntimeSymbol("praxis_max_heap_push"),
        doc: "Push a value onto the max-heap; returns Unit.",
        stability: Stability::Stable,
    }
}

fn max_heap_pop() -> MethodEntry {
    MethodEntry {
        receiver: max_heap_of_t(),
        name: "pop",
        params: vec![],
        result: TypePattern::Var("T"),
        purity: Purity::Impure,
        can_fault: true,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_max_heap_pop"),
        doc: "Remove and return the largest element; faults if empty.",
        stability: Stability::Stable,
    }
}

fn max_heap_peek() -> MethodEntry {
    MethodEntry {
        receiver: max_heap_of_t(),
        name: "peek",
        params: vec![],
        result: TypePattern::Var("T"),
        purity: Purity::Pure,
        can_fault: true,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_max_heap_peek"),
        doc: "The largest element without removing it; faults if empty.",
        stability: Stability::Stable,
    }
}

fn max_heap_len() -> MethodEntry {
    MethodEntry {
        receiver: max_heap_of_t(),
        name: "len",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_max_heap_len"),
        doc: "Number of elements in the max-heap.",
        stability: Stability::Stable,
    }
}

fn max_heap_is_empty() -> MethodEntry {
    MethodEntry {
        receiver: max_heap_of_t(),
        name: "is_empty",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Bool),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_max_heap_is_empty"),
        doc: "True iff the max-heap has no elements.",
        stability: Stability::Stable,
    }
}

fn min_heap_push() -> MethodEntry {
    MethodEntry {
        receiver: min_heap_of_t(),
        name: "push",
        params: vec![TypePattern::Var("T")],
        result: TypePattern::Unit,
        purity: Purity::Impure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::RuntimeSymbol("praxis_min_heap_push"),
        doc: "Push a value onto the min-heap; returns Unit.",
        stability: Stability::Stable,
    }
}

fn min_heap_pop() -> MethodEntry {
    MethodEntry {
        receiver: min_heap_of_t(),
        name: "pop",
        params: vec![],
        result: TypePattern::Var("T"),
        purity: Purity::Impure,
        can_fault: true,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_min_heap_pop"),
        doc: "Remove and return the smallest element; faults if empty.",
        stability: Stability::Stable,
    }
}

fn min_heap_peek() -> MethodEntry {
    MethodEntry {
        receiver: min_heap_of_t(),
        name: "peek",
        params: vec![],
        result: TypePattern::Var("T"),
        purity: Purity::Pure,
        can_fault: true,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_min_heap_peek"),
        doc: "The smallest element without removing it; faults if empty.",
        stability: Stability::Stable,
    }
}

fn min_heap_len() -> MethodEntry {
    MethodEntry {
        receiver: min_heap_of_t(),
        name: "len",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_min_heap_len"),
        doc: "Number of elements in the min-heap.",
        stability: Stability::Stable,
    }
}

fn min_heap_is_empty() -> MethodEntry {
    MethodEntry {
        receiver: min_heap_of_t(),
        name: "is_empty",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Bool),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_min_heap_is_empty"),
        doc: "True iff the min-heap has no elements.",
        stability: Stability::Stable,
    }
}

// --- BitSet methods (M8-WS5, §6.1) --------------------------------------

/// The `BitSet` receiver pattern (nullary — no type args).
fn bitset_receiver() -> TypePattern {
    TypePattern::Collection {
        ctor: CollectionCtor::BitSet,
        args: vec![],
    }
}

fn bitset_insert() -> MethodEntry {
    MethodEntry {
        receiver: bitset_receiver(),
        name: "insert",
        params: vec![TypePattern::Scalar(ScalarType::Int)],
        result: TypePattern::Unit,
        purity: Purity::Impure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::RuntimeSymbol("praxis_bitset_insert"),
        doc: "Set the bit for a non-negative integer; returns Unit.",
        stability: Stability::Stable,
    }
}

fn bitset_remove() -> MethodEntry {
    MethodEntry {
        receiver: bitset_receiver(),
        name: "remove",
        params: vec![TypePattern::Scalar(ScalarType::Int)],
        result: TypePattern::Unit,
        purity: Purity::Impure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_bitset_remove"),
        doc: "Clear the bit for an integer; returns Unit.",
        stability: Stability::Stable,
    }
}

fn bitset_contains() -> MethodEntry {
    MethodEntry {
        receiver: bitset_receiver(),
        name: "contains",
        params: vec![TypePattern::Scalar(ScalarType::Int)],
        result: TypePattern::Scalar(ScalarType::Bool),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_bitset_contains"),
        doc: "True iff the bit for the integer is set.",
        stability: Stability::Stable,
    }
}

fn bitset_len() -> MethodEntry {
    MethodEntry {
        receiver: bitset_receiver(),
        name: "len",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_bitset_len"),
        doc: "Number of set bits (popcount).",
        stability: Stability::Stable,
    }
}

fn bitset_is_empty() -> MethodEntry {
    MethodEntry {
        receiver: bitset_receiver(),
        name: "is_empty",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Bool),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_bitset_is_empty"),
        doc: "True iff no bits are set.",
        stability: Stability::Stable,
    }
}

// --- Grid[T] methods (M8-WS5, §6.4) -------------------------------------

/// The `Grid[T]` receiver pattern.
fn grid_of_t() -> TypePattern {
    TypePattern::Collection {
        ctor: CollectionCtor::Grid,
        args: vec![TypePattern::Var("T")],
    }
}

/// A `(x, y)` point: the `(Int, Int)` tuple shape returned by grid methods.
fn point_pattern() -> TypePattern {
    TypePattern::Tuple(vec![
        TypePattern::Scalar(ScalarType::Int),
        TypePattern::Scalar(ScalarType::Int),
    ])
}

fn grid_width() -> MethodEntry {
    MethodEntry {
        receiver: grid_of_t(),
        name: "width",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_grid_width"),
        doc: "The number of columns.",
        stability: Stability::Stable,
    }
}

fn grid_height() -> MethodEntry {
    MethodEntry {
        receiver: grid_of_t(),
        name: "height",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_grid_height"),
        doc: "The number of rows.",
        stability: Stability::Stable,
    }
}

fn grid_get() -> MethodEntry {
    MethodEntry {
        receiver: grid_of_t(),
        name: "get",
        params: vec![
            TypePattern::Scalar(ScalarType::Int),
            TypePattern::Scalar(ScalarType::Int),
        ],
        result: TypePattern::Var("T"),
        purity: Purity::Pure,
        can_fault: true,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_grid_get"),
        doc: "The cell at (x, y); faults if out of range.",
        stability: Stability::Stable,
    }
}

fn grid_set() -> MethodEntry {
    MethodEntry {
        receiver: grid_of_t(),
        name: "set",
        params: vec![
            TypePattern::Scalar(ScalarType::Int),
            TypePattern::Scalar(ScalarType::Int),
            TypePattern::Var("T"),
        ],
        result: TypePattern::Unit,
        purity: Purity::Impure,
        can_fault: true,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_grid_set"),
        doc: "Set the cell at (x, y); faults if out of range.",
        stability: Stability::Stable,
    }
}

fn grid_contains() -> MethodEntry {
    MethodEntry {
        receiver: grid_of_t(),
        name: "contains",
        params: vec![
            TypePattern::Scalar(ScalarType::Int),
            TypePattern::Scalar(ScalarType::Int),
        ],
        result: TypePattern::Scalar(ScalarType::Bool),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::RuntimeSymbol("praxis_grid_contains"),
        doc: "True iff (x, y) is within the grid.",
        stability: Stability::Stable,
    }
}

fn grid_neighbors4() -> MethodEntry {
    MethodEntry {
        receiver: grid_of_t(),
        name: "neighbors4",
        params: vec![point_pattern()],
        result: TypePattern::Collection {
            ctor: CollectionCtor::Vec,
            args: vec![point_pattern()],
        },
        purity: Purity::Pure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::RuntimeSymbol("praxis_grid_neighbors4"),
        doc: "The 4 orthogonal in-bounds neighbors of a point, as a Vec of (x, y).",
        stability: Stability::Stable,
    }
}

fn grid_neighbors8() -> MethodEntry {
    MethodEntry {
        receiver: grid_of_t(),
        name: "neighbors8",
        params: vec![point_pattern()],
        result: TypePattern::Collection {
            ctor: CollectionCtor::Vec,
            args: vec![point_pattern()],
        },
        purity: Purity::Pure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::RuntimeSymbol("praxis_grid_neighbors8"),
        doc: "The 8 in-bounds neighbors of a point, as a Vec of (x, y).",
        stability: Stability::Stable,
    }
}

fn grid_positions() -> MethodEntry {
    MethodEntry {
        receiver: grid_of_t(),
        name: "positions",
        params: vec![],
        result: TypePattern::Collection {
            ctor: CollectionCtor::Vec,
            args: vec![point_pattern()],
        },
        purity: Purity::Pure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::RuntimeSymbol("praxis_grid_positions"),
        doc: "All (x, y) positions in row-major order, as a Vec.",
        stability: Stability::Stable,
    }
}

fn grid_cells() -> MethodEntry {
    MethodEntry {
        receiver: grid_of_t(),
        name: "cells",
        params: vec![],
        result: TypePattern::Collection {
            ctor: CollectionCtor::Vec,
            args: vec![TypePattern::Var("T")],
        },
        purity: Purity::Pure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::RuntimeSymbol("praxis_grid_cells"),
        doc: "All cells in row-major order, as a Vec.",
        stability: Stability::Stable,
    }
}

fn grid_row() -> MethodEntry {
    MethodEntry {
        receiver: grid_of_t(),
        name: "row",
        params: vec![TypePattern::Scalar(ScalarType::Int)],
        result: TypePattern::Collection {
            ctor: CollectionCtor::Vec,
            args: vec![TypePattern::Var("T")],
        },
        purity: Purity::Pure,
        can_fault: true,
        allocates: true,
        lowering: MethodLowering::RuntimeSymbol("praxis_grid_row"),
        doc: "Row `y` as a Vec; faults if out of range.",
        stability: Stability::Stable,
    }
}

fn grid_column() -> MethodEntry {
    MethodEntry {
        receiver: grid_of_t(),
        name: "column",
        params: vec![TypePattern::Scalar(ScalarType::Int)],
        result: TypePattern::Collection {
            ctor: CollectionCtor::Vec,
            args: vec![TypePattern::Var("T")],
        },
        purity: Purity::Pure,
        can_fault: true,
        allocates: true,
        lowering: MethodLowering::RuntimeSymbol("praxis_grid_column"),
        doc: "Column `x` as a Vec; faults if out of range.",
        stability: Stability::Stable,
    }
}

fn grid_find() -> MethodEntry {
    MethodEntry {
        receiver: grid_of_t(),
        name: "find",
        params: vec![TypePattern::Var("T")],
        result: point_pattern(),
        purity: Purity::Pure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::RuntimeSymbol("praxis_grid_find"),
        doc: "The first (x, y) whose cell equals `value`, or Unit if none.",
        stability: Stability::Stable,
    }
}

fn grid_find_all() -> MethodEntry {
    MethodEntry {
        receiver: grid_of_t(),
        name: "find_all",
        params: vec![TypePattern::Var("T")],
        result: TypePattern::Collection {
            ctor: CollectionCtor::Vec,
            args: vec![point_pattern()],
        },
        purity: Purity::Pure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::RuntimeSymbol("praxis_grid_find_all"),
        doc: "All (x, y) positions whose cell equals `value`, as a Vec.",
        stability: Stability::Stable,
    }
}

fn grid_transpose() -> MethodEntry {
    MethodEntry {
        receiver: grid_of_t(),
        name: "transpose",
        params: vec![],
        result: grid_of_t(),
        purity: Purity::Pure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::RuntimeSymbol("praxis_grid_transpose"),
        doc: "A transposed copy (rows ↔ columns).",
        stability: Stability::Stable,
    }
}

fn grid_rotate_left() -> MethodEntry {
    MethodEntry {
        receiver: grid_of_t(),
        name: "rotate_left",
        params: vec![],
        result: grid_of_t(),
        purity: Purity::Pure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::RuntimeSymbol("praxis_grid_rotate_left"),
        doc: "A copy rotated 90° counter-clockwise.",
        stability: Stability::Stable,
    }
}

fn grid_rotate_right() -> MethodEntry {
    MethodEntry {
        receiver: grid_of_t(),
        name: "rotate_right",
        params: vec![],
        result: grid_of_t(),
        purity: Purity::Pure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::RuntimeSymbol("praxis_grid_rotate_right"),
        doc: "A copy rotated 90° clockwise.",
        stability: Stability::Stable,
    }
}

// --- Pipeline combinators (M8-WS8, §6.3) ---------------------------------
// The functional-sequence pipeline. `Seq[T]` is a compiler-internal lazy type
// (no runtime representation); the combinators are intrinsics the compiler
// fuses into a single loop over the source. Streaming combinators (map/filter)
// are defined on BOTH Vec[T] and Seq[T] so a chain can start on a concrete
// collection and continue on Seq. Sinks (sum/count/collect/fold) terminate.

/// The `Seq[T]` receiver pattern (compiler-internal, §6.3).
fn seq_of_t() -> TypePattern {
    TypePattern::Collection {
        ctor: CollectionCtor::Seq,
        args: vec![TypePattern::Var("T")],
    }
}

/// `(T) -> U` — the shape of `map`'s closure argument.
fn t_to_u() -> TypePattern {
    TypePattern::Function {
        params: vec![TypePattern::Var("T")],
        result: Box::new(TypePattern::Var("U")),
    }
}

/// `(T) -> Bool` — the shape of `filter`'s predicate.
fn t_to_bool() -> TypePattern {
    TypePattern::Function {
        params: vec![TypePattern::Var("T")],
        result: Box::new(TypePattern::Scalar(ScalarType::Bool)),
    }
}

/// `(Acc, T) -> Acc` — the shape of `fold`'s combining closure.
fn acc_t_to_acc() -> TypePattern {
    TypePattern::Function {
        params: vec![TypePattern::Var("Acc"), TypePattern::Var("T")],
        result: Box::new(TypePattern::Var("Acc")),
    }
}

fn seq_map_on_vec() -> MethodEntry {
    MethodEntry {
        receiver: vec_of_t(),
        name: "map",
        params: vec![t_to_u()],
        result: vec_of_u(),
        purity: Purity::Pure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::Intrinsic("seq_map"),
        doc: "Apply a function to each element, collecting into a Vec.",
        stability: Stability::Stable,
    }
}

fn seq_map_on_seq() -> MethodEntry {
    MethodEntry {
        receiver: seq_of_t(),
        name: "map",
        params: vec![t_to_u()],
        result: vec_of_u(),
        purity: Purity::Pure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::Intrinsic("seq_map"),
        doc: "Apply a function to each element, collecting into a Vec.",
        stability: Stability::Stable,
    }
}

/// `Vec[U]` — the result of a `map` (a fresh element variable U). The current
/// implementation eagerly materializes to Vec; true lazy `Seq[U]` with cross-
/// combinator fusion is the documented next refinement (M8-WS8 continuation).
fn vec_of_u() -> TypePattern {
    TypePattern::Collection {
        ctor: CollectionCtor::Vec,
        args: vec![TypePattern::Var("U")],
    }
}

fn seq_filter_on_vec() -> MethodEntry {
    MethodEntry {
        receiver: vec_of_t(),
        name: "filter",
        params: vec![t_to_bool()],
        result: vec_of_t(),
        purity: Purity::Pure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::Intrinsic("seq_filter"),
        doc: "Keep elements satisfying a predicate, collecting into a Vec.",
        stability: Stability::Stable,
    }
}

fn seq_filter_on_seq() -> MethodEntry {
    MethodEntry {
        receiver: seq_of_t(),
        name: "filter",
        params: vec![t_to_bool()],
        result: vec_of_t(),
        purity: Purity::Pure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::Intrinsic("seq_filter"),
        doc: "Keep elements satisfying a predicate, collecting into a Vec.",
        stability: Stability::Stable,
    }
}

fn seq_fold_on_vec() -> MethodEntry {
    MethodEntry {
        receiver: vec_of_t(),
        name: "fold",
        params: vec![TypePattern::Var("Acc"), acc_t_to_acc()],
        result: TypePattern::Var("Acc"),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::Intrinsic("seq_fold"),
        doc: "Reduce elements left-to-right with an accumulator and combining closure.",
        stability: Stability::Stable,
    }
}

fn seq_fold_on_seq() -> MethodEntry {
    MethodEntry {
        receiver: seq_of_t(),
        name: "fold",
        params: vec![TypePattern::Var("Acc"), acc_t_to_acc()],
        result: TypePattern::Var("Acc"),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::Intrinsic("seq_fold"),
        doc: "Reduce elements left-to-right with an accumulator and combining closure.",
        stability: Stability::Stable,
    }
}

fn seq_sum_on_vec() -> MethodEntry {
    MethodEntry {
        receiver: vec_of_t(),
        name: "sum",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::Intrinsic("seq_sum"),
        doc: "Sum the (Int) elements.",
        stability: Stability::Stable,
    }
}

fn seq_sum_on_seq() -> MethodEntry {
    MethodEntry {
        receiver: seq_of_t(),
        name: "sum",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::Intrinsic("seq_sum"),
        doc: "Sum the (Int) elements.",
        stability: Stability::Stable,
    }
}

fn seq_count_on_vec() -> MethodEntry {
    MethodEntry {
        receiver: vec_of_t(),
        name: "count",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::Intrinsic("seq_count"),
        doc: "Number of elements.",
        stability: Stability::Stable,
    }
}

fn seq_count_on_seq() -> MethodEntry {
    MethodEntry {
        receiver: seq_of_t(),
        name: "count",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::Intrinsic("seq_count"),
        doc: "Number of elements.",
        stability: Stability::Stable,
    }
}

fn seq_collect_on_vec() -> MethodEntry {
    MethodEntry {
        receiver: vec_of_t(),
        name: "collect",
        params: vec![],
        result: vec_of_t(),
        purity: Purity::Pure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::Intrinsic("seq_collect"),
        doc: "Materialize the elements into a Vec.",
        stability: Stability::Stable,
    }
}

fn seq_collect_on_seq() -> MethodEntry {
    MethodEntry {
        receiver: seq_of_t(),
        name: "collect",
        params: vec![],
        result: vec_of_t(),
        purity: Purity::Pure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::Intrinsic("seq_collect"),
        doc: "Materialize the elements into a Vec.",
        stability: Stability::Stable,
    }
}

// --- M8-WS11: the remaining non-barrier combinators (§6.3) ----------------
// Each is an intrinsic lowered by the MIR fuser (`recognize_pipeline` +
// `lower_pipeline`) into a single fused loop. Defined on both `Vec[T]` and
// `Seq[T]` so chains can start on a concrete collection and continue on Seq.
// The 5 barriers (sorted/unique/frequencies/chunks/windows) need new runtime
// helpers and are intentionally NOT registered here — they Y110 until that
// separate workstream lands.

/// `(T, T) -> Bool` — the shape of `min_by`/`max_by`'s comparator ("less-than").
fn t_t_to_bool() -> TypePattern {
    TypePattern::Function {
        params: vec![TypePattern::Var("T"), TypePattern::Var("T")],
        result: Box::new(TypePattern::Scalar(ScalarType::Bool)),
    }
}

/// `(T) -> Vec<U>` — the shape of `flat_map`'s closure.
fn t_to_vec_u() -> TypePattern {
    TypePattern::Function {
        params: vec![TypePattern::Var("T")],
        result: Box::new(vec_of_u()),
    }
}

// Streaming stages ---------------------------------------------------------

fn seq_take_on_vec() -> MethodEntry {
    MethodEntry {
        receiver: vec_of_t(),
        name: "take",
        params: vec![TypePattern::Scalar(ScalarType::Int)],
        result: vec_of_t(),
        purity: Purity::Pure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::Intrinsic("seq_take"),
        doc: "Keep at most the first n elements.",
        stability: Stability::Stable,
    }
}

fn seq_take_on_seq() -> MethodEntry {
    MethodEntry {
        receiver: seq_of_t(),
        name: "take",
        params: vec![TypePattern::Scalar(ScalarType::Int)],
        result: vec_of_t(),
        purity: Purity::Pure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::Intrinsic("seq_take"),
        doc: "Keep at most the first n elements.",
        stability: Stability::Stable,
    }
}

fn seq_skip_on_vec() -> MethodEntry {
    MethodEntry {
        receiver: vec_of_t(),
        name: "skip",
        params: vec![TypePattern::Scalar(ScalarType::Int)],
        result: vec_of_t(),
        purity: Purity::Pure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::Intrinsic("seq_skip"),
        doc: "Drop the first n elements.",
        stability: Stability::Stable,
    }
}

fn seq_skip_on_seq() -> MethodEntry {
    MethodEntry {
        receiver: seq_of_t(),
        name: "skip",
        params: vec![TypePattern::Scalar(ScalarType::Int)],
        result: vec_of_t(),
        purity: Purity::Pure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::Intrinsic("seq_skip"),
        doc: "Drop the first n elements.",
        stability: Stability::Stable,
    }
}

fn seq_take_while_on_vec() -> MethodEntry {
    MethodEntry {
        receiver: vec_of_t(),
        name: "take_while",
        params: vec![t_to_bool()],
        result: vec_of_t(),
        purity: Purity::Pure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::Intrinsic("seq_take_while"),
        doc: "Keep elements until the predicate is false.",
        stability: Stability::Stable,
    }
}

fn seq_take_while_on_seq() -> MethodEntry {
    MethodEntry {
        receiver: seq_of_t(),
        name: "take_while",
        params: vec![t_to_bool()],
        result: vec_of_t(),
        purity: Purity::Pure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::Intrinsic("seq_take_while"),
        doc: "Keep elements until the predicate is false.",
        stability: Stability::Stable,
    }
}

fn seq_enumerate_on_vec() -> MethodEntry {
    MethodEntry {
        receiver: vec_of_t(),
        name: "enumerate",
        params: vec![],
        result: vec_of_t(),
        purity: Purity::Pure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::Intrinsic("seq_enumerate"),
        doc: "Pair each element with its index.",
        stability: Stability::Stable,
    }
}

fn seq_enumerate_on_seq() -> MethodEntry {
    MethodEntry {
        receiver: seq_of_t(),
        name: "enumerate",
        params: vec![],
        result: vec_of_t(),
        purity: Purity::Pure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::Intrinsic("seq_enumerate"),
        doc: "Pair each element with its index.",
        stability: Stability::Stable,
    }
}

fn seq_zip_on_vec() -> MethodEntry {
    MethodEntry {
        receiver: vec_of_t(),
        name: "zip",
        params: vec![vec_of_t()],
        result: vec_of_t(),
        purity: Purity::Pure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::Intrinsic("seq_zip"),
        doc: "Pair elements with another sequence, stopping at the shorter length.",
        stability: Stability::Stable,
    }
}

fn seq_zip_on_seq() -> MethodEntry {
    MethodEntry {
        receiver: seq_of_t(),
        name: "zip",
        params: vec![vec_of_t()],
        result: vec_of_t(),
        purity: Purity::Pure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::Intrinsic("seq_zip"),
        doc: "Pair elements with another sequence, stopping at the shorter length.",
        stability: Stability::Stable,
    }
}

fn seq_flat_map_on_vec() -> MethodEntry {
    MethodEntry {
        receiver: vec_of_t(),
        name: "flat_map",
        params: vec![t_to_vec_u()],
        result: vec_of_u(),
        purity: Purity::Pure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::Intrinsic("seq_flat_map"),
        doc: "Map each element to a Vec and concatenate the results.",
        stability: Stability::Stable,
    }
}

fn seq_flat_map_on_seq() -> MethodEntry {
    MethodEntry {
        receiver: seq_of_t(),
        name: "flat_map",
        params: vec![t_to_vec_u()],
        result: vec_of_u(),
        purity: Purity::Pure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::Intrinsic("seq_flat_map"),
        doc: "Map each element to a Vec and concatenate the results.",
        stability: Stability::Stable,
    }
}

fn seq_filter_map_on_vec() -> MethodEntry {
    MethodEntry {
        receiver: vec_of_t(),
        name: "filter_map",
        params: vec![t_to_u()],
        result: vec_of_u(),
        purity: Purity::Pure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::Intrinsic("seq_filter_map"),
        doc: "Map and drop Unit results (modeled as map-keep for non-Unit results).",
        stability: Stability::Stable,
    }
}

fn seq_filter_map_on_seq() -> MethodEntry {
    MethodEntry {
        receiver: seq_of_t(),
        name: "filter_map",
        params: vec![t_to_u()],
        result: vec_of_u(),
        purity: Purity::Pure,
        can_fault: false,
        allocates: true,
        lowering: MethodLowering::Intrinsic("seq_filter_map"),
        doc: "Map and drop Unit results (modeled as map-keep for non-Unit results).",
        stability: Stability::Stable,
    }
}

// Aggregating sinks (scalar result) ---------------------------------------

fn seq_product_on_vec() -> MethodEntry {
    MethodEntry {
        receiver: vec_of_t(),
        name: "product",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::Intrinsic("seq_product"),
        doc: "Multiply the (Int) elements.",
        stability: Stability::Stable,
    }
}

fn seq_product_on_seq() -> MethodEntry {
    MethodEntry {
        receiver: seq_of_t(),
        name: "product",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::Intrinsic("seq_product"),
        doc: "Multiply the (Int) elements.",
        stability: Stability::Stable,
    }
}

fn seq_min_on_vec() -> MethodEntry {
    MethodEntry {
        receiver: vec_of_t(),
        name: "min",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::Intrinsic("seq_min"),
        doc: "Smallest (Int) element; the first element seeds the accumulator.",
        stability: Stability::Stable,
    }
}

fn seq_min_on_seq() -> MethodEntry {
    MethodEntry {
        receiver: seq_of_t(),
        name: "min",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::Intrinsic("seq_min"),
        doc: "Smallest (Int) element; the first element seeds the accumulator.",
        stability: Stability::Stable,
    }
}

fn seq_max_on_vec() -> MethodEntry {
    MethodEntry {
        receiver: vec_of_t(),
        name: "max",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::Intrinsic("seq_max"),
        doc: "Largest (Int) element; the first element seeds the accumulator.",
        stability: Stability::Stable,
    }
}

fn seq_max_on_seq() -> MethodEntry {
    MethodEntry {
        receiver: seq_of_t(),
        name: "max",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::Intrinsic("seq_max"),
        doc: "Largest (Int) element; the first element seeds the accumulator.",
        stability: Stability::Stable,
    }
}

fn seq_min_by_on_vec() -> MethodEntry {
    MethodEntry {
        receiver: vec_of_t(),
        name: "min_by",
        params: vec![t_t_to_bool()],
        result: TypePattern::Var("T"),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::Intrinsic("seq_min_by"),
        doc: "Smallest element per a (T,T)->Bool \"less-than\" comparator.",
        stability: Stability::Stable,
    }
}

fn seq_min_by_on_seq() -> MethodEntry {
    MethodEntry {
        receiver: seq_of_t(),
        name: "min_by",
        params: vec![t_t_to_bool()],
        result: TypePattern::Var("T"),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::Intrinsic("seq_min_by"),
        doc: "Smallest element per a (T,T)->Bool \"less-than\" comparator.",
        stability: Stability::Stable,
    }
}

fn seq_max_by_on_vec() -> MethodEntry {
    MethodEntry {
        receiver: vec_of_t(),
        name: "max_by",
        params: vec![t_t_to_bool()],
        result: TypePattern::Var("T"),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::Intrinsic("seq_max_by"),
        doc: "Largest element per a (T,T)->Bool \"less-than\" comparator.",
        stability: Stability::Stable,
    }
}

fn seq_max_by_on_seq() -> MethodEntry {
    MethodEntry {
        receiver: seq_of_t(),
        name: "max_by",
        params: vec![t_t_to_bool()],
        result: TypePattern::Var("T"),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::Intrinsic("seq_max_by"),
        doc: "Largest element per a (T,T)->Bool \"less-than\" comparator.",
        stability: Stability::Stable,
    }
}

fn seq_any_on_vec() -> MethodEntry {
    MethodEntry {
        receiver: vec_of_t(),
        name: "any",
        params: vec![t_to_bool()],
        result: TypePattern::Scalar(ScalarType::Bool),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::Intrinsic("seq_any"),
        doc: "True if any element satisfies the predicate (short-circuits).",
        stability: Stability::Stable,
    }
}

fn seq_any_on_seq() -> MethodEntry {
    MethodEntry {
        receiver: seq_of_t(),
        name: "any",
        params: vec![t_to_bool()],
        result: TypePattern::Scalar(ScalarType::Bool),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::Intrinsic("seq_any"),
        doc: "True if any element satisfies the predicate (short-circuits).",
        stability: Stability::Stable,
    }
}

fn seq_all_on_vec() -> MethodEntry {
    MethodEntry {
        receiver: vec_of_t(),
        name: "all",
        params: vec![t_to_bool()],
        result: TypePattern::Scalar(ScalarType::Bool),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::Intrinsic("seq_all"),
        doc: "True if all elements satisfy the predicate (short-circuits).",
        stability: Stability::Stable,
    }
}

fn seq_all_on_seq() -> MethodEntry {
    MethodEntry {
        receiver: seq_of_t(),
        name: "all",
        params: vec![t_to_bool()],
        result: TypePattern::Scalar(ScalarType::Bool),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::Intrinsic("seq_all"),
        doc: "True if all elements satisfy the predicate (short-circuits).",
        stability: Stability::Stable,
    }
}

fn seq_find_on_vec() -> MethodEntry {
    MethodEntry {
        receiver: vec_of_t(),
        name: "find",
        params: vec![t_to_bool()],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::Intrinsic("seq_find"),
        doc: "Index of the first matching element, or -1 on miss.",
        stability: Stability::Stable,
    }
}

fn seq_find_on_seq() -> MethodEntry {
    MethodEntry {
        receiver: seq_of_t(),
        name: "find",
        params: vec![t_to_bool()],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::Intrinsic("seq_find"),
        doc: "Index of the first matching element, or -1 on miss.",
        stability: Stability::Stable,
    }
}

fn seq_position_on_vec() -> MethodEntry {
    MethodEntry {
        receiver: vec_of_t(),
        name: "position",
        params: vec![t_to_bool()],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::Intrinsic("seq_position"),
        doc: "Index of the first matching element, or -1 on miss (alias of find).",
        stability: Stability::Stable,
    }
}

fn seq_position_on_seq() -> MethodEntry {
    MethodEntry {
        receiver: seq_of_t(),
        name: "position",
        params: vec![t_to_bool()],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::Intrinsic("seq_position"),
        doc: "Index of the first matching element, or -1 on miss (alias of find).",
        stability: Stability::Stable,
    }
}

fn seq_reduce_on_vec() -> MethodEntry {
    MethodEntry {
        receiver: vec_of_t(),
        name: "reduce",
        params: vec![acc_t_to_acc()],
        result: TypePattern::Var("T"),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::Intrinsic("seq_reduce"),
        doc: "Reduce left-to-right, seeded with the first element.",
        stability: Stability::Stable,
    }
}

fn seq_reduce_on_seq() -> MethodEntry {
    MethodEntry {
        receiver: seq_of_t(),
        name: "reduce",
        params: vec![acc_t_to_acc()],
        result: TypePattern::Var("T"),
        purity: Purity::Pure,
        can_fault: false,
        allocates: false,
        lowering: MethodLowering::Intrinsic("seq_reduce"),
        doc: "Reduce left-to-right, seeded with the first element.",
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

    /// M8-WS7 closed-catalog check: every §6.1 collection has at least the
    /// `len`/`is_empty` pair, plus its type-specific methods. This guards against
    /// an accidental catalog gap where a collection ships without its methods.
    #[test]
    fn catalog_covers_every_collection_kind() {
        let cat = builtin_catalog();
        // Each collection must have a `len` and `is_empty` method (or the
        // type-specific equivalent — heaps have len/is_empty; bitset has them too).
        for (ctor, name) in [
            (CollectionCtor::Vec, "Vec"),
            (CollectionCtor::Deque, "Deque"),
            (CollectionCtor::Set, "Set"),
            (CollectionCtor::Counter, "Counter"),
            (CollectionCtor::MinHeap, "MinHeap"),
            (CollectionCtor::MaxHeap, "MaxHeap"),
            (CollectionCtor::BitSet, "BitSet"),
        ] {
            let args: Vec<TypePattern> = match ctor.arity() {
                0 => Vec::new(),
                n => (0..n).map(|_| TypePattern::Var("T")).collect(),
            };
            let pat = TypePattern::Collection { ctor, args };
            let len = cat.by_receiver_and_name(&pat, "len").count();
            let is_empty = cat.by_receiver_and_name(&pat, "is_empty").count();
            assert!(len >= 1, "{name} missing len method");
            assert!(is_empty >= 1, "{name} missing is_empty method");
        }
        // Map has two type args with distinct var names (K, V).
        let map_pat = TypePattern::Collection {
            ctor: CollectionCtor::Map,
            args: vec![TypePattern::Var("K"), TypePattern::Var("V")],
        };
        assert!(cat.by_receiver_and_name(&map_pat, "len").count() >= 1);
        assert!(cat.by_receiver_and_name(&map_pat, "is_empty").count() >= 1);
        // Grid has width/height (its dimension methods).
        let grid_pat = TypePattern::Collection {
            ctor: CollectionCtor::Grid,
            args: vec![TypePattern::Var("T")],
        };
        assert!(cat.by_receiver_and_name(&grid_pat, "width").count() >= 1);
        assert!(cat.by_receiver_and_name(&grid_pat, "neighbors4").count() >= 1);
    }
}
