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

use crate::abi;
use crate::catalog::MethodCatalog;
use crate::type_pattern::{CollectionCtor, ScalarType};
use crate::{MethodEntry, MethodLowering, Purity, Stability, TypePattern};

/// The `Vec[T]` receiver pattern, used by every Vec method entry.
fn vec_of_t() -> TypePattern {
    TypePattern::Collection {
        ctor: CollectionCtor::Vec,
        args: vec![TypePattern::var("T")],
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
        .entry(vec_to_text())
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
        // Pipeline combinators (M8-WS8, §6.3, ADR-127). Intrinsics the compiler
        // fuses into a single loop over the source. **One row apiece**, on the
        // generic `Iterable` receiver: the language already had a complete answer
        // to "what can I iterate and what does it yield" — `capability::iter_item`
        // for eleven collections plus `Text` — and this is the pipeline reading
        // it instead of keeping a second, much smaller one. The `Vec`/`Seq` pair
        // these replace was forty-nine rows of which twenty-three were dead.
        .entry(seq_map())
        .entry(seq_filter())
        .entry(seq_fold())
        .entry(seq_sum())
        .entry(seq_count())
        .entry(seq_count_if())
        // The barrier combinators (§6.3). Runtime symbols rather than
        // intrinsics — see the block comment above their definitions.
        .entry(seq_sorted())
        .entry(seq_sorted_by_key())
        .entry(seq_unique())
        .entry(seq_reversed())
        .entry(seq_frequencies())
        .entry(seq_join())
        // The two groupings (ADR-149). Barriers too, and the only rows whose
        // result nests a collection inside a collection.
        .entry(seq_chunks())
        .entry(seq_windows())
        // M8-WS11: the remaining non-barrier combinators. Each is an intrinsic
        // fused by the MIR pipeline recognizer.
        .entry(seq_take())
        .entry(seq_skip())
        .entry(seq_take_while())
        .entry(seq_enumerate())
        .entry(seq_zip())
        .entry(seq_flat_map())
        .entry(seq_filter_map())
        .entry(seq_product())
        .entry(seq_min())
        .entry(seq_max())
        .entry(seq_min_by())
        .entry(seq_max_by())
        .entry(seq_any())
        .entry(seq_all())
        .entry(seq_find())
        .entry(seq_position())
        .entry(seq_reduce())
        // The conversions (ADR-127 decision 4). Fused sinks, one per collection
        // with a constructor — a pipeline's currency is `Vec`, and a program
        // that wants a collection back says which one.
        .entry(seq_to_vec())
        .entry(seq_to_set())
        .entry(seq_to_map())
        .entry(seq_to_counter())
        .entry(seq_to_deque())
        .entry(seq_to_min_heap())
        .entry(seq_to_max_heap())
        .entry(seq_to_bitset())
        .entry(text_len())
        .entry(text_int())
        .entry(text_float())
        .entry(text_is_empty())
        .entry(text_get())
        // Float methods (§4.12). Pure unary math, predicates, conversions, and
        // binary min/max — all lower to `praxis_float_*` runtime wrappers.
        .entry(float_abs())
        .entry(float_sqrt())
        .entry(float_floor())
        .entry(float_ceil())
        .entry(float_round())
        .entry(float_sign())
        .entry(float_to_int())
        .entry(float_to_text())
        .entry(float_is_nan())
        .entry(float_is_infinite())
        .entry(float_min())
        .entry(float_max())
        // The explicit Int→Float widening method (§4.12). The first Int-receiver
        // method; establishes the pattern for scalar-receiver methods.
        .entry(int_to_float())
        // The Char/Int conversion pair (ADR-086), written as a pair for the
        // reason §4.12 writes Float.to_int/Int.to_float as one.
        .entry(char_to_int())
        .entry(int_to_char())
        // The other two thirds of the `to_text` family (ADR-143). `Float`'s row
        // is above; all three share one renderer with `out`.
        .entry(int_to_text())
        .entry(char_to_text())
        .entry(int_wrapping_add())
        .entry(int_saturating_add())
        .entry(int_checked_add())
        .entry(int_wrapping_sub())
        .entry(int_saturating_sub())
        .entry(int_checked_sub())
        .entry(int_wrapping_mul())
        .entry(int_saturating_mul())
        .entry(int_checked_mul())
        // Subscripts (REP-16, §4.7/§6.2/§6.4). Six collections read; five of
        // those six also store — every one but the immutable `Text`. See the
        // block comment above `vec_index` for why these are catalog rows.
        .entry(vec_index())
        .entry(vec_index_set())
        .entry(deque_index())
        .entry(deque_index_set())
        .entry(text_index())
        .entry(map_index())
        .entry(map_index_set())
        .entry(counter_index())
        .entry(counter_index_set())
        .entry(grid_index())
        .entry(grid_index_set())
        // …and the two updating stores §6.2 writes (REP-21). Map only: they are
        // the two wrappers that exist, and an absent entry accepting the first
        // value is a semantics no read-modify-write over the rows above can
        // express, because a subscript read of an absent key faults (§4.7).
        .entry(map_index_min())
        .entry(map_index_max())
        // Keyed enumeration (REP-18). §3.3's `counts.values()`, plus the `Map`
        // siblings — the only way to enumerate a `Map` while REP-15 stands.
        .entry(counter_keys())
        .entry(counter_values())
        .entry(map_keys())
        .entry(map_values())
        .finish()
        .expect("built-in catalog must be duplicate-free")
}

// --- Keyed enumeration (REP-18) ----------------------------------------------
//
// §3.3's representative program ends in `counts.values().count(|n| n >= 2)`, and
// `values` existed nowhere: not in the catalog, and nowhere else in the design doc
// either. These four rows are it, plus the `Map` siblings — which are also the only
// way to enumerate a `Map` today, because `for kv in m` has no lowering at all
// (REP-15).
//
// Each answers a `Vec`, so every §6.3 pipeline combinator applies to the result.
// The order is fixed and deterministic (by the key's rendered form), so `keys()`
// and `values()` are index-aligned and a program's *answer* cannot depend on a
// `HashMap`'s per-process seed.

fn counter_keys() -> MethodEntry {
    MethodEntry {
        receiver: counter_of_t(),
        name: "keys",
        params: vec![],
        result: TypePattern::Collection {
            ctor: CollectionCtor::Vec,
            args: vec![TypePattern::var("T")],
        },
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::CounterKeys),
        doc: "Every key, as a `Vec[T]`, ordered with `values()`.",
        stability: Stability::Stable,
    }
}

fn counter_values() -> MethodEntry {
    MethodEntry {
        receiver: counter_of_t(),
        name: "values",
        params: vec![],
        result: TypePattern::Collection {
            ctor: CollectionCtor::Vec,
            args: vec![TypePattern::Scalar(ScalarType::Int)],
        },
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::CounterValues),
        doc: "Every count, as a `Vec[Int]`, ordered with `keys()`.",
        stability: Stability::Stable,
    }
}

fn map_keys() -> MethodEntry {
    MethodEntry {
        receiver: map_of_k_v(),
        name: "keys",
        params: vec![],
        result: TypePattern::Collection {
            ctor: CollectionCtor::Vec,
            args: vec![TypePattern::var("K")],
        },
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::MapKeys),
        doc: "Every key, as a `Vec[K]`, ordered with `values()`.",
        stability: Stability::Stable,
    }
}

fn map_values() -> MethodEntry {
    MethodEntry {
        receiver: map_of_k_v(),
        name: "values",
        params: vec![],
        result: TypePattern::Collection {
            ctor: CollectionCtor::Vec,
            args: vec![TypePattern::var("V")],
        },
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::MapValues),
        doc: "Every value, as a `Vec[V]`, ordered with `keys()`.",
        stability: Stability::Stable,
    }
}

// --- Subscript rows (REP-16, §4.7/§6.2/§6.4) ---------------------------------
//
// `m[key]`, `counts[key] += 1` and `grid[x, y]` dispatch through the catalog on
// the receiver's shape and the index count, which is what a method call already
// does. Their names — `[]`, `[]=` — are not identifiers, so no program can spell
// them; the subscript grammar is their only caller.
//
// Which collections index is a language decision and this table is where it is
// recorded. Six read: `Vec`, `Deque`, `Text`, `Map`, `Counter`, `Grid`. **Five
// store** — every reader but `Text`, which is immutable (§4.3), so `t[0] = c` is
// still the report `not_index_assignable` gives.
//
// `Vec` and `Deque` were the two readers with no store, which ADR-064 recorded
// as a feature owed rather than a gap it would smuggle in; they have one now,
// through `praxis_vec_set`/`praxis_deque_set`. Their stores **replace** and never
// append: `v[v.len()] = x` is `IndexOutOfBounds` rather than a push, so an
// off-by-one is reported instead of growing the vector.
//
// The read rows repeat their `get` sibling's symbol on purpose — except `Map`,
// whose two answers differ by design: `.get` returns Unit for an absent key and
// `map[key]` **faults** (§4.7), so it has its own wrapper.

fn vec_index() -> MethodEntry {
    MethodEntry {
        receiver: vec_of_t(),
        name: crate::catalog::INDEX_READ,
        params: vec![TypePattern::Scalar(ScalarType::Int)],
        result: TypePattern::var("T"),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::VecGet),
        doc: "`v[i]` — the element at `i`; faults if out of range.",
        stability: Stability::Stable,
    }
}

fn vec_index_set() -> MethodEntry {
    MethodEntry {
        receiver: vec_of_t(),
        name: crate::catalog::INDEX_STORE,
        params: vec![TypePattern::Scalar(ScalarType::Int), TypePattern::var("T")],
        result: TypePattern::Unit,
        purity: Purity::Impure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::VecSet),
        doc: "`v[i] = value` — replace the element at `i`; faults if out of range \
              (it never appends — `push` is the spelling that grows a vector).",
        stability: Stability::Stable,
    }
}

fn deque_index() -> MethodEntry {
    MethodEntry {
        receiver: deque_of_t(),
        name: crate::catalog::INDEX_READ,
        params: vec![TypePattern::Scalar(ScalarType::Int)],
        result: TypePattern::var("T"),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::DequeGet),
        doc: "`d[i]` — the element at `i` (0-based from the front); faults if out of range.",
        stability: Stability::Stable,
    }
}

fn deque_index_set() -> MethodEntry {
    MethodEntry {
        receiver: deque_of_t(),
        name: crate::catalog::INDEX_STORE,
        params: vec![TypePattern::Scalar(ScalarType::Int), TypePattern::var("T")],
        result: TypePattern::Unit,
        purity: Purity::Impure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::DequeSet),
        doc: "`d[i] = value` — replace the element at `i` (0-based from the front); \
              faults if out of range (it never inserts).",
        stability: Stability::Stable,
    }
}

fn text_index() -> MethodEntry {
    MethodEntry {
        receiver: text_receiver(),
        name: crate::catalog::INDEX_READ,
        params: vec![TypePattern::Scalar(ScalarType::Int)],
        result: TypePattern::Scalar(ScalarType::Char),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::TextGet),
        doc: "`t[i]` — the `Char` at `i`, indexing by Unicode scalar value and not \
              by byte; faults if out of range (ADR-086).",
        stability: Stability::Stable,
    }
}

fn map_index() -> MethodEntry {
    MethodEntry {
        receiver: map_of_k_v(),
        name: crate::catalog::INDEX_READ,
        params: vec![TypePattern::var("K")],
        result: TypePattern::var("V"),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::MapIndex),
        doc: "`m[key]` — the value for `key`; **faults** if absent (§4.7; `.get` is the \
              spelling that answers with absence).",
        stability: Stability::Stable,
    }
}

fn map_index_set() -> MethodEntry {
    MethodEntry {
        receiver: map_of_k_v(),
        name: crate::catalog::INDEX_STORE,
        params: vec![TypePattern::var("K"), TypePattern::var("V")],
        result: TypePattern::Unit,
        purity: Purity::Impure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::MapInsert),
        doc: "`m[key] = value` — set `key`, replacing any prior value.",
        stability: Stability::Stable,
    }
}

/// The `Map[K, V]` receiver of `min=`/`max=`, whose value is bound to `Int`.
///
/// A **bound** rather than a literal `Int` argument, for TY-31's reason: the
/// bound *pins* an unresolved value type instead of merely permitting it, so
/// `var d = Map()` followed by `d[k] min= 1` gives `d` an `Int` value type rather
/// than reporting. The bound is what the wrapper needs — `praxis_map_update_min`
/// compares through `int_payload`, so a `Map[Text, Text]` would read its values
/// as `i64`s.
fn map_of_k_int_value() -> TypePattern {
    TypePattern::Collection {
        ctor: CollectionCtor::Map,
        args: vec![
            TypePattern::var("K"),
            TypePattern::is_scalar("V", ScalarType::Int),
        ],
    }
}

fn map_index_min() -> MethodEntry {
    MethodEntry {
        receiver: map_of_k_int_value(),
        name: crate::catalog::INDEX_STORE_MIN,
        params: vec![TypePattern::var("K"), TypePattern::var("V")],
        result: TypePattern::Unit,
        purity: Purity::Impure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::MapUpdateMin),
        doc: "`d[key] min= candidate` — keep the smaller value; an absent entry \
              accepts the first value (§6.2).",
        stability: Stability::Stable,
    }
}

fn map_index_max() -> MethodEntry {
    MethodEntry {
        receiver: map_of_k_int_value(),
        name: crate::catalog::INDEX_STORE_MAX,
        params: vec![TypePattern::var("K"), TypePattern::var("V")],
        result: TypePattern::Unit,
        purity: Purity::Impure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::MapUpdateMax),
        doc: "`b[key] max= score` — keep the larger value; an absent entry accepts \
              the first value (§6.2).",
        stability: Stability::Stable,
    }
}

fn counter_index() -> MethodEntry {
    MethodEntry {
        receiver: counter_of_t(),
        name: crate::catalog::INDEX_READ,
        params: vec![TypePattern::var("T")],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::CounterGet),
        doc: "`c[key]` — the count for `key`, or zero if absent (§6.2); never faults.",
        stability: Stability::Stable,
    }
}

fn counter_index_set() -> MethodEntry {
    MethodEntry {
        receiver: counter_of_t(),
        name: crate::catalog::INDEX_STORE,
        params: vec![TypePattern::var("T"), TypePattern::Scalar(ScalarType::Int)],
        result: TypePattern::Unit,
        purity: Purity::Impure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::CounterSet),
        doc: "`c[key] = n` — set the count for `key`.",
        stability: Stability::Stable,
    }
}

fn grid_index() -> MethodEntry {
    MethodEntry {
        receiver: grid_of_t(),
        name: crate::catalog::INDEX_READ,
        params: vec![
            TypePattern::Scalar(ScalarType::Int),
            TypePattern::Scalar(ScalarType::Int),
        ],
        result: TypePattern::var("T"),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::GridGet),
        doc: "`grid[x, y]` — the cell at (x, y) (§6.4); faults if out of range.",
        stability: Stability::Stable,
    }
}

fn grid_index_set() -> MethodEntry {
    MethodEntry {
        receiver: grid_of_t(),
        name: crate::catalog::INDEX_STORE,
        params: vec![
            TypePattern::Scalar(ScalarType::Int),
            TypePattern::Scalar(ScalarType::Int),
            TypePattern::var("T"),
        ],
        result: TypePattern::Unit,
        purity: Purity::Impure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::GridSet),
        doc: "`grid[x, y] = value` — set the cell at (x, y); faults if out of range.",
        stability: Stability::Stable,
    }
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
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::TextLen),
        doc: "Number of Unicode scalar values (chars) in the text.",
        stability: Stability::Stable,
    }
}

/// `Text.int() -> Option[Int]` — the one text-to-number conversion (ADR-136).
///
/// `Y001`'s help on `var count: Int = raw` has read "this is `Text`; call
/// `.int()` on it (or use `read lines(int)`)" since it was written, and only the
/// parenthesized half was true: `Text`'s rows were `len`, `is_empty` and `get`,
/// so `raw.int()` reported `Y110`. Half of the help for the most common mistake
/// in a puzzle program pointed at nothing.
///
/// `Option[Int]` rather than `Int`, for §4.7's reason and `Map.get`'s: a text
/// that is not a number is *absence*, not a fault. Input is routinely not what a
/// program hoped, and a panicking conversion would leave `"abc".int()` a crash
/// with no way to ask first.
fn text_int() -> MethodEntry {
    MethodEntry {
        receiver: text_receiver(),
        name: "int",
        params: vec![],
        result: TypePattern::Option(Box::new(TypePattern::Scalar(ScalarType::Int))),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::TextInt),
        doc: "The Int this text spells as `Some(n)`, or `None` if it spells none.",
        stability: Stability::Stable,
    }
}

/// `Text.float() -> Option[Float]` — [`text_int`]'s twin (ADR-136).
///
/// Same shape and the same reason: a text that is not a number is *absence*, not
/// a fault. The accepted set is §7.4's `float` atomic over the whole trimmed
/// text, so `t.float()` and `parse(t, float)` cannot disagree — which means
/// `"inf"` and `"nan"` are `None`, because neither is a token the input parser
/// reads.
fn text_float() -> MethodEntry {
    MethodEntry {
        receiver: text_receiver(),
        name: "float",
        params: vec![],
        result: TypePattern::Option(Box::new(TypePattern::Scalar(ScalarType::Float))),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::TextFloat),
        doc: "The Float this text spells as `Some(x)`, or `None` if it spells none.",
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
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::TextIsEmpty),
        doc: "True iff the text has no chars.",
        stability: Stability::Stable,
    }
}

fn text_get() -> MethodEntry {
    MethodEntry {
        receiver: text_receiver(),
        name: "get",
        params: vec![TypePattern::Scalar(ScalarType::Int)],
        result: TypePattern::Scalar(ScalarType::Char),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::TextGet),
        doc: "The `Char` at `index`; faults if out of range. `t[index]` is the \
              same row and the same answer (ADR-086).",
        stability: Stability::Stable,
    }
}

// ---- Float methods (§4.12) --------------------------------------------------
//
// All Float method entries share a Float receiver pattern. The pure unary math
// methods (`abs`/`sqrt`/`floor`/`ceil`/`round`/`sign`) and predicates never
// fault; `to_int` is the sole faulting method (NaN/inf/out-of-range). `min`/
// `max` take a Float argument. Conversions return Int/Text.

fn float_receiver() -> TypePattern {
    TypePattern::Scalar(ScalarType::Float)
}

fn float_abs() -> MethodEntry {
    MethodEntry {
        receiver: float_receiver(),
        name: "abs",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Float),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::FloatAbs),
        doc: "Absolute value.",
        stability: Stability::Stable,
    }
}

fn float_sqrt() -> MethodEntry {
    MethodEntry {
        receiver: float_receiver(),
        name: "sqrt",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Float),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::FloatSqrt),
        doc: "Square root. Negative inputs yield NaN (IEEE-754).",
        stability: Stability::Stable,
    }
}

fn float_floor() -> MethodEntry {
    MethodEntry {
        receiver: float_receiver(),
        name: "floor",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Float),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::FloatFloor),
        doc: "Round toward negative infinity.",
        stability: Stability::Stable,
    }
}

fn float_ceil() -> MethodEntry {
    MethodEntry {
        receiver: float_receiver(),
        name: "ceil",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Float),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::FloatCeil),
        doc: "Round toward positive infinity.",
        stability: Stability::Stable,
    }
}

fn float_round() -> MethodEntry {
    MethodEntry {
        receiver: float_receiver(),
        name: "round",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Float),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::FloatRound),
        doc: "Round half away from zero.",
        stability: Stability::Stable,
    }
}

fn float_sign() -> MethodEntry {
    MethodEntry {
        receiver: float_receiver(),
        name: "sign",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Float),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::FloatSign),
        doc: "Sign as -1.0 / 0.0 / 1.0. NaN yields NaN.",
        stability: Stability::Stable,
    }
}

fn float_to_int() -> MethodEntry {
    MethodEntry {
        receiver: float_receiver(),
        name: "to_int",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::FloatToInt),
        doc: "Truncate toward zero to an Int. Faults on NaN, ±inf, or out of i64 range.",
        stability: Stability::Stable,
    }
}

fn float_to_text() -> MethodEntry {
    MethodEntry {
        receiver: float_receiver(),
        name: "to_text",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Text),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::FloatToText),
        doc: "Format as Text (shortest round-trip form; inf/-inf/NaN as literals).",
        stability: Stability::Stable,
    }
}

fn float_is_nan() -> MethodEntry {
    MethodEntry {
        receiver: float_receiver(),
        name: "is_nan",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Bool),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::FloatIsNan),
        doc: "True iff NaN.",
        stability: Stability::Stable,
    }
}

fn float_is_infinite() -> MethodEntry {
    MethodEntry {
        receiver: float_receiver(),
        name: "is_infinite",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Bool),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::FloatIsInfinite),
        doc: "True iff ±infinity.",
        stability: Stability::Stable,
    }
}

fn float_min() -> MethodEntry {
    MethodEntry {
        receiver: float_receiver(),
        name: "min",
        params: vec![TypePattern::Scalar(ScalarType::Float)],
        result: TypePattern::Scalar(ScalarType::Float),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::FloatMin),
        doc: "The smaller of two floats. If either is NaN, returns the other.",
        stability: Stability::Stable,
    }
}

fn float_max() -> MethodEntry {
    MethodEntry {
        receiver: float_receiver(),
        name: "max",
        params: vec![TypePattern::Scalar(ScalarType::Float)],
        result: TypePattern::Scalar(ScalarType::Float),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::FloatMax),
        doc: "The larger of two floats. If either is NaN, returns the other.",
        stability: Stability::Stable,
    }
}

fn int_to_float() -> MethodEntry {
    MethodEntry {
        receiver: TypePattern::Scalar(ScalarType::Int),
        name: "to_float",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Float),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::IntToFloat),
        doc: "Widen to Float (explicit Int→Float conversion, §4.12).",
        stability: Stability::Stable,
    }
}

// --- the `to_text` family (ADR-143) -----------------------------------------
//
// Three rows — `Int`, `Float`, `Char` — and the family is closed at three.
//
// **Each answers the characters `out` writes, by construction.** The wrapper
// behind each row calls the same `scalars::write_*` function the type
// descriptor's `format` callback calls, so there is one renderer per scalar with
// two callers rather than two renderers that have to be kept in agreement. A
// program that prints a value and a program that builds a `Text` from it
// disagreeing is the defect this shape makes unrepresentable; §4.12's
// shortest-round-trip rule for `Float` is the one that would have drifted first.
//
// **Deliberately absent, each for its own reason**, in the convention the Char
// conversion block below uses:
//
// - **`Bool.to_text()`.** No design-doc surface asks for one, which is what
//   REP-46 refused to invent rows past. `if b { "true" } else { "false" }` says
//   it, and says which spelling the program wanted.
// - **A universal `T.to_text()`.** That is §8.1 interpolation's question, not
//   this one: a hole that stringifies *any* value needs a rendering conversion
//   defined on every type, which is the implicit conversion to `Text` that
//   ADR-085 decision 2 refused for `+`. It wants its own decision.
//
// §8.1's interpolation itself stays specified and unimplemented. These rows make
// it cheaper rather than redundant — with `Int`, `Float`, `Char` and `Text` all
// covered, `"a{n}b"` can desugar to `"a" + n.to_text() + "b"` and needs no new
// runtime path.

fn int_to_text() -> MethodEntry {
    MethodEntry {
        receiver: TypePattern::Scalar(ScalarType::Int),
        name: "to_text",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Text),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::IntToText),
        doc: "Format as Text — the same digits `out` writes (ADR-143).",
        stability: Stability::Stable,
    }
}

// --- Char conversions (ADR-086) ----------------------------------------------
//
// The `Char`/`Int` pair, written as a pair for the reason §4.12 writes
// `Float.to_int`/`Int.to_float` as one: a one-way conversion is a one-way door.
// With `to_int` alone a program could take a `Char` apart and never build one,
// so `Grid[Char]`, `Vec[Char]` and `Map[Char, _]` would stay write-only from the
// language's side.
//
// `to_int` is required and not a nicety. Before ADR-086 a text index answered an
// `Int`, so `t[i] - 48`, `t[i] >= 97` and a `Map[Int, _]` keyed on a character
// were all ordinary; `capability::supports_numeric` excludes `Char` on purpose
// ("a `Char` is a scalar value and not an arithmetic one"), so without this row
// making the index a `Char` would be a straight regression. With it, every
// program expressible before stays expressible by inserting `.to_int()`.
//
// **Deliberately absent, each for its own reason** — the same convention the
// `_add` trio's comment below uses, so an omission is recorded where a reader
// looks for it rather than only in a commit message:
//
// - **`is_digit`, `is_alpha`, `to_upper`, `to_lower`.** No design-doc surface
//   asks for any of them and `to_int()` expresses every one. Four invented rows
//   is exactly what REP-46 refused with `wrapping_sub`/`_mul`.
// - **`Text.chars()`.** `for c in text` **is** the spelling (ADR-099): a `Text`
//   is iterable and yields the same `Char` `t[i]` answers, through the same
//   `praxis_text_len`/`praxis_text_get` pair. A `chars()` row would be a second
//   spelling for one question, which is what ADR-077 refused. This note used to
//   record the `for` as a gap alongside it; that half is closed.
//
// **`Char.to_text()` used to be listed here and no longer is** (ADR-143). The
// argument for its absence was that it gives a second spelling for "is this
// character a `#`" — `t[i].to_text() == "#"` beside `t[i] == "#"[0]` — and that
// ADR-077 refuses two spellings for one question. It does, but this is not one:
// ADR-077 refused two *accessors*, and the comparison through `to_text` is not a
// second way to ask the question so much as a worse one, allocating a `Text` to
// answer what a scalar compare already answers. The question `to_text` is here
// for is the other direction — producing output text — and that had no spelling
// at all.

fn char_to_int() -> MethodEntry {
    MethodEntry {
        receiver: TypePattern::Scalar(ScalarType::Char),
        name: "to_int",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::CharToInt),
        doc: "The Unicode scalar value, as an `Int`. Never faults (ADR-086).",
        stability: Stability::Stable,
    }
}

fn char_to_text() -> MethodEntry {
    MethodEntry {
        receiver: TypePattern::Scalar(ScalarType::Char),
        name: "to_text",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Text),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::CharToText),
        doc: "The one-character Text holding this scalar — the same character \
              `out` writes. Never faults (ADR-143).",
        stability: Stability::Stable,
    }
}

fn int_to_char() -> MethodEntry {
    MethodEntry {
        receiver: TypePattern::Scalar(ScalarType::Int),
        name: "to_char",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Char),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::IntToChar),
        doc: "The `Char` with this Unicode scalar value; **faults** \
              (`InvalidChar`) if it is negative, above `0x10FFFF`, or a \
              surrogate. The narrowing half of the pair, as `Float.to_int` is \
              (ADR-086).",
        stability: Stability::Stable,
    }
}

// §4.12's three explicit overflow alternatives (REP-46). The design document
// writes exactly these three spellings, and until now none of them existed:
// §4.12 said "integer arithmetic is checked by default … explicit alternatives"
// and then named three methods a program could not call, so the section
// described a language with no way to opt out of a fault.
//
// **The family is three modes over three operators** — `wrapping_`,
// `saturating_`, `checked_` × `add`, `sub`, `mul`. §4.12 states that shape and
// both of its closures (no `_div`/`_rem`, no `_neg`/`_abs`) and is the only
// place the rule is written; `the_overflow_alternative_family_is_three_modes_over_three_operators`
// below is what enforces it against this table.
//
// The `_sub`/`_mul` six were once deliberately absent, on the reading that
// §4.12 "names only the three". That reading does not survive checking: the
// sentence it rests on was written **by REP-46's own first half** as a note
// deferring the question, so it cannot be the authority for the decision it was
// written to defer. §4.12 closes a set in prose every time it means to — "The
// stdlib Float methods **are** …", "Division by zero always faults" — and closes
// nothing here. The measurement decided the rest: `wrapping_mul` cannot be
// written in this language at all (every arithmetic operator faults and there
// are no bitwise operators), so leaving it out was a hole with no reason
// behind it.

fn int_wrapping_add() -> MethodEntry {
    MethodEntry {
        receiver: TypePattern::Scalar(ScalarType::Int),
        name: "wrapping_add",
        params: vec![TypePattern::Scalar(ScalarType::Int)],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::IntWrappingAdd),
        doc: "Add with two's-complement wraparound instead of a fault (§4.12).",
        stability: Stability::Stable,
    }
}

fn int_saturating_add() -> MethodEntry {
    MethodEntry {
        receiver: TypePattern::Scalar(ScalarType::Int),
        name: "saturating_add",
        params: vec![TypePattern::Scalar(ScalarType::Int)],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::IntSaturatingAdd),
        doc: "Add, clamping to Int's ends instead of faulting (§4.12).",
        stability: Stability::Stable,
    }
}

fn int_checked_add() -> MethodEntry {
    MethodEntry {
        receiver: TypePattern::Scalar(ScalarType::Int),
        name: "checked_add",
        params: vec![TypePattern::Scalar(ScalarType::Int)],
        result: TypePattern::Option(Box::new(TypePattern::Scalar(ScalarType::Int))),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::IntCheckedAdd),
        doc: "Add, answering None where the checked `+` would fault (§4.12).",
        stability: Stability::Stable,
    }
}

fn int_wrapping_sub() -> MethodEntry {
    MethodEntry {
        receiver: TypePattern::Scalar(ScalarType::Int),
        name: "wrapping_sub",
        params: vec![TypePattern::Scalar(ScalarType::Int)],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::IntWrappingSub),
        doc: "Subtract with two's-complement wraparound instead of a fault (§4.12).",
        stability: Stability::Stable,
    }
}

fn int_saturating_sub() -> MethodEntry {
    MethodEntry {
        receiver: TypePattern::Scalar(ScalarType::Int),
        name: "saturating_sub",
        params: vec![TypePattern::Scalar(ScalarType::Int)],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::IntSaturatingSub),
        doc: "Subtract, clamping to Int's ends instead of faulting (§4.12).",
        stability: Stability::Stable,
    }
}

fn int_checked_sub() -> MethodEntry {
    MethodEntry {
        receiver: TypePattern::Scalar(ScalarType::Int),
        name: "checked_sub",
        params: vec![TypePattern::Scalar(ScalarType::Int)],
        result: TypePattern::Option(Box::new(TypePattern::Scalar(ScalarType::Int))),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::IntCheckedSub),
        doc: "Subtract, answering None where the checked `-` would fault (§4.12).",
        stability: Stability::Stable,
    }
}

fn int_wrapping_mul() -> MethodEntry {
    MethodEntry {
        receiver: TypePattern::Scalar(ScalarType::Int),
        name: "wrapping_mul",
        params: vec![TypePattern::Scalar(ScalarType::Int)],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::IntWrappingMul),
        doc: "Multiply with two's-complement wraparound instead of a fault. The \
              one row here a program could not write for itself: every arithmetic \
              operator is checked and the language has no bitwise operators \
              (§4.12).",
        stability: Stability::Stable,
    }
}

fn int_saturating_mul() -> MethodEntry {
    MethodEntry {
        receiver: TypePattern::Scalar(ScalarType::Int),
        name: "saturating_mul",
        params: vec![TypePattern::Scalar(ScalarType::Int)],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::IntSaturatingMul),
        doc: "Multiply, clamping to Int's ends instead of faulting (§4.12).",
        stability: Stability::Stable,
    }
}

fn int_checked_mul() -> MethodEntry {
    MethodEntry {
        receiver: TypePattern::Scalar(ScalarType::Int),
        name: "checked_mul",
        params: vec![TypePattern::Scalar(ScalarType::Int)],
        result: TypePattern::Option(Box::new(TypePattern::Scalar(ScalarType::Int))),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::IntCheckedMul),
        doc: "Multiply, answering None where the checked `*` would fault (§4.12).",
        stability: Stability::Stable,
    }
}

fn vec_push() -> MethodEntry {
    MethodEntry {
        receiver: vec_of_t(),
        name: "push",
        params: vec![TypePattern::var("T")],
        result: TypePattern::Unit,
        purity: Purity::Impure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::VecPush),
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
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::VecLen),
        doc: "Number of elements in the vector.",
        stability: Stability::Stable,
    }
}

fn vec_get() -> MethodEntry {
    MethodEntry {
        receiver: vec_of_t(),
        name: "get",
        params: vec![TypePattern::Scalar(ScalarType::Int)],
        result: TypePattern::var("T"),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::VecGet),
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
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::VecIsEmpty),
        doc: "True iff the vector has no elements.",
        stability: Stability::Stable,
    }
}

/// `chars.to_text()` — a sequence of `Char`s as one `Text` (ADR-144), which is
/// what renders `g.row(y)` back as the line it was read from.
///
/// **On `Vec[Char]` and not on the generic `Iterable` receiver**, which is the
/// one thing about this row a reader cannot re-derive. `Text` is one of the ten
/// pipeline receivers, so an `Iterable.to_text/0` row would sit at `(name,
/// arity)` beside every scalar `to_text` the catalog has and beside any future
/// `Text.to_text` — and `MethodCatalogBuilder::finish` answers
/// `AmbiguousWithIterable` to exactly that. A concrete `Vec` receiver is safe
/// against all of them, and a `Set[Char]` or a `Grid[Char]` row would be
/// answering a different question anyway: a sequence of characters becomes a
/// line because it has an *order*.
///
/// The element is a **bounded variable** rather than a literal `Char`, for
/// `map_of_k_int_value`'s reason: the bound pins an unresolved element type
/// instead of merely permitting it, so `var v = Vec()` followed by `v.to_text()`
/// gives `v` a `Char` element type, and `[1, 2].to_text()` reports `expected
/// Char, found Int` at the method name rather than "no method `to_text`".
fn vec_to_text() -> MethodEntry {
    MethodEntry {
        receiver: TypePattern::Collection {
            ctor: CollectionCtor::Vec,
            args: vec![TypePattern::is_scalar("T", ScalarType::Char)],
        },
        name: "to_text",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Text),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::VecToText),
        doc: "These Chars as one Text, with nothing between them (ADR-144).",
        stability: Stability::Stable,
    }
}

// --- Deque methods (M8-WS2, §6.1) ----------------------------------------

/// The `Deque[T]` receiver pattern, used by every Deque method entry.
fn deque_of_t() -> TypePattern {
    TypePattern::Collection {
        ctor: CollectionCtor::Deque,
        args: vec![TypePattern::var("T")],
    }
}

fn deque_push_front() -> MethodEntry {
    MethodEntry {
        receiver: deque_of_t(),
        name: "push_front",
        params: vec![TypePattern::var("T")],
        result: TypePattern::Unit,
        purity: Purity::Impure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::DequePushFront),
        doc: "Prepend a value to the front; returns Unit.",
        stability: Stability::Stable,
    }
}

fn deque_push_back() -> MethodEntry {
    MethodEntry {
        receiver: deque_of_t(),
        name: "push_back",
        params: vec![TypePattern::var("T")],
        result: TypePattern::Unit,
        purity: Purity::Impure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::DequePushBack),
        doc: "Append a value to the back; returns Unit.",
        stability: Stability::Stable,
    }
}

fn deque_pop_front() -> MethodEntry {
    MethodEntry {
        receiver: deque_of_t(),
        name: "pop_front",
        params: vec![],
        result: TypePattern::var("T"),
        purity: Purity::Impure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::DequePopFront),
        doc: "Remove and return the front element; faults if empty.",
        stability: Stability::Stable,
    }
}

fn deque_pop_back() -> MethodEntry {
    MethodEntry {
        receiver: deque_of_t(),
        name: "pop_back",
        params: vec![],
        result: TypePattern::var("T"),
        purity: Purity::Impure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::DequePopBack),
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
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::DequeLen),
        doc: "Number of elements in the deque.",
        stability: Stability::Stable,
    }
}

fn deque_get() -> MethodEntry {
    MethodEntry {
        receiver: deque_of_t(),
        name: "get",
        params: vec![TypePattern::Scalar(ScalarType::Int)],
        result: TypePattern::var("T"),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::DequeGet),
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
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::DequeIsEmpty),
        doc: "True iff the deque has no elements.",
        stability: Stability::Stable,
    }
}

// --- Map / Set / Counter methods (M8-WS3, §6.1, §11.3) -------------------

/// The `Map[K, V]` receiver pattern: two type args (key, value).
fn map_of_k_v() -> TypePattern {
    TypePattern::Collection {
        ctor: CollectionCtor::Map,
        args: vec![TypePattern::var("K"), TypePattern::var("V")],
    }
}

/// The `Set[T]` receiver pattern.
fn set_of_t() -> TypePattern {
    TypePattern::Collection {
        ctor: CollectionCtor::Set,
        args: vec![TypePattern::var("T")],
    }
}

/// The `Counter[T]` receiver pattern (key type only; values are Int).
fn counter_of_t() -> TypePattern {
    TypePattern::Collection {
        ctor: CollectionCtor::Counter,
        args: vec![TypePattern::var("T")],
    }
}

fn map_insert() -> MethodEntry {
    MethodEntry {
        receiver: map_of_k_v(),
        name: "insert",
        params: vec![TypePattern::var("K"), TypePattern::var("V")],
        result: TypePattern::Unit,
        purity: Purity::Impure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::MapInsert),
        doc: "Set `key` to `value`, replacing any prior value; returns Unit.",
        stability: Stability::Stable,
    }
}

fn map_get() -> MethodEntry {
    MethodEntry {
        receiver: map_of_k_v(),
        name: "get",
        params: vec![TypePattern::var("K")],
        // §5.7 writes this signature literally: `Map[K,V].get(K) -> Option[V]`.
        // The row said `V` and the wrapper answered the Unit sentinel on a miss
        // (RT-14), which is a value whose static type is `V` and whose runtime
        // descriptor is `Unit`. §4.7: absence is `Option`, and `map[key]` is
        // the assertion-like half that faults.
        result: TypePattern::Option(Box::new(TypePattern::var("V"))),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::MapGet),
        doc: "The value for `key` as `Some(value)`, or `None` if absent.",
        stability: Stability::Stable,
    }
}

fn map_contains() -> MethodEntry {
    MethodEntry {
        receiver: map_of_k_v(),
        name: "contains",
        params: vec![TypePattern::var("K")],
        result: TypePattern::Scalar(ScalarType::Bool),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::MapContains),
        doc: "True iff `key` is present in the map.",
        stability: Stability::Stable,
    }
}

fn map_remove() -> MethodEntry {
    MethodEntry {
        receiver: map_of_k_v(),
        name: "remove",
        params: vec![TypePattern::var("K")],
        result: TypePattern::Unit,
        purity: Purity::Impure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::MapRemove),
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
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::MapLen),
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
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::MapIsEmpty),
        doc: "True iff the map has no entries.",
        stability: Stability::Stable,
    }
}

fn set_insert() -> MethodEntry {
    MethodEntry {
        receiver: set_of_t(),
        name: "insert",
        params: vec![TypePattern::var("T")],
        result: TypePattern::Unit,
        purity: Purity::Impure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::SetInsert),
        doc: "Add `value` to the set; returns Unit.",
        stability: Stability::Stable,
    }
}

fn set_remove() -> MethodEntry {
    MethodEntry {
        receiver: set_of_t(),
        name: "remove",
        params: vec![TypePattern::var("T")],
        result: TypePattern::Unit,
        purity: Purity::Impure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::SetRemove),
        doc: "Remove `value` if present; returns Unit.",
        stability: Stability::Stable,
    }
}

fn set_contains() -> MethodEntry {
    MethodEntry {
        receiver: set_of_t(),
        name: "contains",
        params: vec![TypePattern::var("T")],
        result: TypePattern::Scalar(ScalarType::Bool),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::SetContains),
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
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::SetLen),
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
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::SetIsEmpty),
        doc: "True iff the set has no elements.",
        stability: Stability::Stable,
    }
}

fn counter_get() -> MethodEntry {
    MethodEntry {
        receiver: counter_of_t(),
        name: "get",
        params: vec![TypePattern::var("T")],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::CounterGet),
        doc: "The count for `key`, or zero if absent (never faults).",
        stability: Stability::Stable,
    }
}

fn counter_inc() -> MethodEntry {
    MethodEntry {
        receiver: counter_of_t(),
        name: "inc",
        params: vec![TypePattern::var("T")],
        result: TypePattern::Unit,
        purity: Purity::Impure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::CounterInc),
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
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::CounterLen),
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
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::CounterIsEmpty),
        doc: "True iff the counter has no keys.",
        stability: Stability::Stable,
    }
}

// --- MinHeap[T] / MaxHeap[T] methods (M8-WS4, §6.1) ---------------------

fn min_heap_of_t() -> TypePattern {
    TypePattern::Collection {
        ctor: CollectionCtor::MinHeap,
        args: vec![TypePattern::var("T")],
    }
}

fn max_heap_of_t() -> TypePattern {
    TypePattern::Collection {
        ctor: CollectionCtor::MaxHeap,
        args: vec![TypePattern::var("T")],
    }
}

fn max_heap_push() -> MethodEntry {
    MethodEntry {
        receiver: max_heap_of_t(),
        name: "push",
        params: vec![TypePattern::var("T")],
        result: TypePattern::Unit,
        purity: Purity::Impure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::MaxHeapPush),
        doc: "Push a value onto the max-heap; returns Unit.",
        stability: Stability::Stable,
    }
}

fn max_heap_pop() -> MethodEntry {
    MethodEntry {
        receiver: max_heap_of_t(),
        name: "pop",
        params: vec![],
        result: TypePattern::var("T"),
        purity: Purity::Impure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::MaxHeapPop),
        doc: "Remove and return the largest element; faults if empty.",
        stability: Stability::Stable,
    }
}

fn max_heap_peek() -> MethodEntry {
    MethodEntry {
        receiver: max_heap_of_t(),
        name: "peek",
        params: vec![],
        result: TypePattern::var("T"),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::MaxHeapPeek),
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
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::MaxHeapLen),
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
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::MaxHeapIsEmpty),
        doc: "True iff the max-heap has no elements.",
        stability: Stability::Stable,
    }
}

fn min_heap_push() -> MethodEntry {
    MethodEntry {
        receiver: min_heap_of_t(),
        name: "push",
        params: vec![TypePattern::var("T")],
        result: TypePattern::Unit,
        purity: Purity::Impure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::MinHeapPush),
        doc: "Push a value onto the min-heap; returns Unit.",
        stability: Stability::Stable,
    }
}

fn min_heap_pop() -> MethodEntry {
    MethodEntry {
        receiver: min_heap_of_t(),
        name: "pop",
        params: vec![],
        result: TypePattern::var("T"),
        purity: Purity::Impure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::MinHeapPop),
        doc: "Remove and return the smallest element; faults if empty.",
        stability: Stability::Stable,
    }
}

fn min_heap_peek() -> MethodEntry {
    MethodEntry {
        receiver: min_heap_of_t(),
        name: "peek",
        params: vec![],
        result: TypePattern::var("T"),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::MinHeapPeek),
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
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::MinHeapLen),
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
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::MinHeapIsEmpty),
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
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::BitsetInsert),
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
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::BitsetRemove),
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
        // The one `ScalarPrimitive` row in the catalog (ADR-118 decision 6):
        // `bs.contains(x)` lowers to `Inst::BitsetContains`, whose result is a
        // `Scalar(Bool)` and whose out-of-line form is this wrapper.
        lowering: MethodLowering::ScalarPrimitive(abi::RuntimeSymbol::BitsetContains),
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
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::BitsetLen),
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
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::BitsetIsEmpty),
        doc: "True iff no bits are set.",
        stability: Stability::Stable,
    }
}

// --- Grid[T] methods (M8-WS5, §6.4) -------------------------------------

/// The `Grid[T]` receiver pattern.
fn grid_of_t() -> TypePattern {
    TypePattern::Collection {
        ctor: CollectionCtor::Grid,
        args: vec![TypePattern::var("T")],
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
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::GridWidth),
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
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::GridHeight),
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
        result: TypePattern::var("T"),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::GridGet),
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
            TypePattern::var("T"),
        ],
        result: TypePattern::Unit,
        purity: Purity::Impure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::GridSet),
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
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::GridContains),
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
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::GridNeighbors4),
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
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::GridNeighbors8),
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
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::GridPositions),
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
            args: vec![TypePattern::var("T")],
        },
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::GridCells),
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
            args: vec![TypePattern::var("T")],
        },
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::GridRow),
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
            args: vec![TypePattern::var("T")],
        },
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::GridColumn),
        doc: "Column `x` as a Vec; faults if out of range.",
        stability: Stability::Stable,
    }
}

fn grid_find() -> MethodEntry {
    MethodEntry {
        receiver: grid_of_t(),
        name: "find",
        params: vec![TypePattern::var("T")],
        // Absence is `Option`, not the Unit sentinel under a `(Int, Int)`
        // static type (RT-15, §4.7). `find_all` needs no such thing — a `Vec`
        // already encodes "nothing matched" as emptiness.
        result: TypePattern::Option(Box::new(point_pattern())),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::GridFind),
        doc: "The first (x, y) whose cell equals `value` as `Some((x, y))`, or `None`.",
        stability: Stability::Stable,
    }
}

fn grid_find_all() -> MethodEntry {
    MethodEntry {
        receiver: grid_of_t(),
        name: "find_all",
        params: vec![TypePattern::var("T")],
        result: TypePattern::Collection {
            ctor: CollectionCtor::Vec,
            args: vec![point_pattern()],
        },
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::GridFindAll),
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
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::GridTranspose),
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
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::GridRotateLeft),
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
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::GridRotateRight),
        doc: "A copy rotated 90° clockwise.",
        stability: Stability::Stable,
    }
}

// --- Pipeline combinators (M8-WS8, §6.3, ADR-127) -------------------------
// The functional-sequence pipeline: intrinsics the compiler fuses into a single
// loop over the source. Sinks (sum/count/fold) terminate, and a chain that ends
// without one materializes anyway (ADR-126).
//
// **A pipeline's receiver is anything a `for` loop can walk**, and it yields
// what the `for` loop's variable would bind (ADR-127 decision 1). One row per
// combinator, on `TypePattern::Iterable`, whose ten accepted receivers are
// `PIPELINE_RECEIVERS` plus `Text`. What replaced: a `Vec[T]` row and a `Seq[T]`
// twin apiece, of which every `Seq` twin was dead — no catalog row ever answered
// a `Seq`, so nothing could reach one.
//
// The receiver generalizes; **two parameters deliberately do not.** `zip`'s
// argument is a `Vec[U]` and `flat_map`'s closure answers one, because the fused
// loop indexes each of them with `praxis_vec_len`/`praxis_vec_get` directly and
// neither has an `IterPlan` in scope. `m.zip(s)` on a `Set` is a unification
// failure at the argument and the spelling is `m.zip(s.to_vec())`;
// `MethodCatalogBuilder::finish` refuses a row that generalizes either.

/// The generic pipeline receiver at an unconstrained item — `map`, `filter`,
/// `count` and the rest of the twenty-three fused rows.
fn iterable_of_t() -> TypePattern {
    TypePattern::iterable(TypePattern::var("T"))
}

/// `(T) -> U` — the shape of `map`'s closure argument.
fn t_to_u() -> TypePattern {
    TypePattern::Function {
        params: vec![TypePattern::var("T")],
        result: Box::new(TypePattern::var("U")),
    }
}

/// `(T) -> Bool` — the shape of `filter`'s predicate.
fn t_to_bool() -> TypePattern {
    TypePattern::Function {
        params: vec![TypePattern::var("T")],
        result: Box::new(TypePattern::Scalar(ScalarType::Bool)),
    }
}

/// `(T) -> Option[U]` — the shape of `filter_map`'s closure argument (REP-38).
///
/// It was `(T) -> U`, which is `map`'s shape and is why `filter_map` lowered as
/// `map`: with an unconstrained `U` there is nothing at runtime that says "this
/// element mapped to nothing", so no filtering was possible and the row's own
/// doc admitted it ("modeled as map-keep"). §6.3 lists `filter_map` with no
/// deferral note, and the row was `Stability::Stable`. S18's `Option` (ADR-076)
/// is what makes the distinction representable: absence is a variant, so the
/// drop test is a tag compare.
fn t_to_option_u() -> TypePattern {
    TypePattern::Function {
        params: vec![TypePattern::var("T")],
        result: Box::new(TypePattern::Option(Box::new(TypePattern::var("U")))),
    }
}

/// `(Acc, T) -> Acc` — the shape of `fold`'s combining closure.
fn acc_t_to_acc() -> TypePattern {
    TypePattern::Function {
        params: vec![TypePattern::var("Acc"), TypePattern::var("T")],
        result: Box::new(TypePattern::var("Acc")),
    }
}

/// `(T, T) -> T` — the shape of `reduce`'s combining closure.
///
/// **`reduce` is `fold` without the seed**, so its accumulator *is* the element
/// type; there is no second variable for it to be. It shared
/// [`acc_t_to_acc`] with `fold`, and the free `Acc` that made sense there was
/// the defect here: nothing tied the closure's first parameter to the element,
/// so `["ab", "c"].reduce(|a, b| a.len())` type-checked with `a` unpinned —
/// `len` resolved against a variable, the closure answered `Int`, and `reduce`
/// answered `Text` at the same time. The two disagreeing is what reached MIR
/// and tripped `build.rs`'s pipeline-recognizer assertion, which was right to
/// fire (REP-68).
fn t_t_to_t() -> TypePattern {
    TypePattern::Function {
        params: vec![TypePattern::var("T"), TypePattern::var("T")],
        result: Box::new(TypePattern::var("T")),
    }
}

fn seq_map() -> MethodEntry {
    MethodEntry {
        receiver: iterable_of_t(),
        name: "map",
        params: vec![t_to_u()],
        result: vec_of_u(),
        purity: Purity::Pure,
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
        args: vec![TypePattern::var("U")],
    }
}

fn seq_filter() -> MethodEntry {
    MethodEntry {
        receiver: iterable_of_t(),
        name: "filter",
        params: vec![t_to_bool()],
        result: vec_of_t(),
        purity: Purity::Pure,
        lowering: MethodLowering::Intrinsic("seq_filter"),
        doc: "Keep elements satisfying a predicate, collecting into a Vec.",
        stability: Stability::Stable,
    }
}

fn seq_fold() -> MethodEntry {
    MethodEntry {
        receiver: iterable_of_t(),
        name: "fold",
        params: vec![TypePattern::var("Acc"), acc_t_to_acc()],
        result: TypePattern::var("Acc"),
        purity: Purity::Pure,
        lowering: MethodLowering::Intrinsic("seq_fold"),
        doc: "Reduce elements left-to-right with an accumulator and combining closure.",
        stability: Stability::Stable,
    }
}

fn seq_sum() -> MethodEntry {
    MethodEntry {
        receiver: iterable_of_int_elem(),
        name: "sum",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        lowering: MethodLowering::Intrinsic("seq_sum"),
        doc: "Sum the (Int) elements.",
        stability: Stability::Stable,
    }
}

fn seq_count() -> MethodEntry {
    MethodEntry {
        receiver: iterable_of_t(),
        name: "count",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        lowering: MethodLowering::Intrinsic("seq_count"),
        doc: "Number of elements.",
        stability: Stability::Stable,
    }
}

/// `v.count(pred)` — §6.3's `count` with a predicate, which is what §3.3 writes
/// (REP-18). A second *arity* of one name, which the catalog's
/// `(receiver, name, arity)` key has always allowed: `count()` is the element
/// count and `count(pred)` the matching-element count.
fn seq_count_if() -> MethodEntry {
    MethodEntry {
        receiver: iterable_of_t(),
        name: "count",
        params: vec![t_to_bool()],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        lowering: MethodLowering::Intrinsic("seq_count"),
        doc: "Number of elements satisfying the predicate.",
        stability: Stability::Stable,
    }
}

// **There is no `collect` row, and ADR-126 is why.** A chain that ends on a
// streaming stage already materializes: `recognize_pipeline` appends the
// `Collect` sink itself, so `v.map(f)` *is* a `Vec[U]` and `v.map(f).collect()`
// built the identical plan. The row was the lazy `Seq[T]` design's bridge, and
// this pipeline is eager (ADR-028 decision 2) — it named a step the compiler
// takes whether or not it is written.

// --- the barrier combinators (§6.3) ---------------------------------------
//
// A barrier needs the whole sequence before it can answer anything, so it
// cannot be fused into the loop feeding it. That makes it the opposite kind of
// row from everything above: a `RuntimeSymbol`, not an `Intrinsic`. That is the
// MIR fuser's guardrail talking and not a style preference — registering
// `sorted` as `MethodLowering::Intrinsic("seq_sorted")` with no
// `classify_link`/`classify_sink` arm was tried, and
// `intrinsics_are_all_recognized_so_there_is_no_second_lowering` answers:
// "`sorted` at arity 0 lowers as an intrinsic and no runtime symbol, but the
// pipeline recognizer declines it — it has no lowering".
//
// `recognize_pipeline` already ends a fused chain at an unclassified
// `MethodCall` and starts a fresh one from its result, which is exactly what a
// barrier means: `pairs.map(f).sorted()` fuses the map into a collect, calls the
// wrapper, and `sorted(…).zip(…)` starts again.
//
// **The receiver is `Iterable` like everything else (ADR-127 decision 3), and
// the lowering materializes it first.** A wrapper needs a real `VecPayload`, so
// `build::emit_iter_vec` puts the plan's snapshot — or, for a receiver with no
// snapshot symbol, a materializing walk — in front of the call. Leaving the
// three on `Vec[T]` was the alternative and it was rejected: it makes
// `set.map(f).sorted()` legal and `set.sorted()` a `Y110`, which is a rule
// nobody can hold in their head.
//
// **`reversed` is a barrier for the definition's own reason** (ADR-145): it
// cannot answer its first element until it has seen the last one. A
// `classify_link` arm would be classified on name and arity alone and applied
// wherever the name appears in a chain, and `v.filter(p).reversed()` does not
// know the filtered length up front — so a fused reverse would be unsound
// anywhere but immediately adjacent to the source. Walking `emit_iter_item` at
// `len - 1 - idx` for that source-adjacent case is a real optimization and a
// deliberate non-goal here: a row has exactly one `MethodLowering`, and the
// general case has to keep working.
//
// **`join` is one row, on the generic receiver, with its item bounded to
// `Text`** (ADR-144). The bound is on the item and not on a second row because
// two `Iterable` rows differing only in their item bound are not duplicates —
// `finish` accepts them, and dispatch would then resolve by insertion order,
// which is the precedence rule ADR-127 decision 6 refuses. A concrete
// `Vec[Char].join/1` beside the generic row is refused outright as
// `AmbiguousWithIterable`. So the sequence-of-`Char` case is a differently-named
// row — `Vec[Char].to_text()`, defined beside `vec_is_empty` — and a
// sequence-of-`Int` joins by rendering first: `ns.map(|n| n.to_text()).join(",")`.
//
// **`chunks` and `windows` answer `Vec[Vec[T]]`, and the descriptor question
// this comment used to defer them over was already answered** (ADR-149). The
// note said the wrappers "have to label the *outer* Vec with `collections::VEC`
// while the inner ones keep the element descriptor", and that sentence is the
// whole rule — there was nothing left to choose. `outer.push(inner)` already
// builds a `Vec[Vec[T]]` and `adopt_or_reject` already labels it `VEC`, so a
// wrapper answering anything else would disagree with `push`; and a wrapper
// naming a label its receiver cannot supply is what `praxis_grid_positions` and
// three siblings have done since M8, with `&tuples::TUPLE`.
//
// What the deferral was really protecting is worth keeping: *don't commit to
// surface no program forces*. That is why these arrive with a program that
// forces them rather than with the three barriers above.
//
// They are barriers for `reversed`'s reason and not for a new one: a grouping
// is a fact about positions in the whole sequence, so neither can answer its
// first group from one element. And they are the two rows in the catalog that
// fault on an *argument* — see [`seq_chunks`].

/// `sorted` — a new `Vec` in ascending order (§6.3).
///
/// The `Ord` bound is the row's own, and it has to be: the wrapper orders
/// through the element descriptor's `compare` callback, and
/// `require_collection_invariants` — which is where the language's other
/// ordering rule lives — is applied to the receiver *type*, where it would be
/// wrong. A `Vec` of unorderable things is a perfectly good `Vec` right up until
/// someone sorts it.
fn seq_sorted() -> MethodEntry {
    MethodEntry {
        receiver: TypePattern::iterable(TypePattern::of_kind("T", crate::CapKind::Ord)),
        name: "sorted",
        params: vec![],
        result: vec_of_t(),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::VecSorted),
        doc: "A new Vec holding these elements in ascending order.",
        stability: Stability::Stable,
    }
}

/// `unique` — a new `Vec` with later duplicates dropped, in first-occurrence
/// order (§6.3).
///
/// `HashStable` and not `Hash`: sameness is decided by the descriptor's `hash`
/// and `equals`, so an element that can change after it has been seen would not
/// be recognized the second time (D4, TY-32).
fn seq_unique() -> MethodEntry {
    MethodEntry {
        receiver: TypePattern::iterable(TypePattern::of_kind("T", crate::CapKind::HashStable)),
        name: "unique",
        params: vec![],
        result: vec_of_t(),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::VecUnique),
        doc: "A new Vec with duplicate elements removed, keeping first occurrences.",
        stability: Stability::Stable,
    }
}

/// `reversed` — a new `Vec` holding these elements back to front (ADR-145).
///
/// **No capability bound at all**, and that is the row's own claim rather than
/// an omission: reversal reads no descriptor callback, so where `sorted` needs
/// `Ord` and `unique` needs `HashStable`, a `Vec` of closures reverses. The
/// wrapper cannot fail either, which is why its manifest row is `Allocates`.
///
/// It answers `Vec[T]` on every one of the ten receivers — including a `Range`,
/// so `for y in (0..n).reversed()` is the countdown. That does *not* reopen
/// ADR-059 decision 3: no descending `Range` value exists, `RangeVal::new` still
/// clamps, and a pipeline's currency is `Vec` (ADR-127 decision 6).
fn seq_reversed() -> MethodEntry {
    MethodEntry {
        receiver: iterable_of_t(),
        name: "reversed",
        params: vec![],
        result: vec_of_t(),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::VecReversed),
        doc: "A new Vec holding these elements in reverse order.",
        stability: Stability::Stable,
    }
}

/// `Vec[Vec[T]]` — what `chunks` and `windows` answer (ADR-149).
///
/// The one result pattern in the catalog that nests a collection inside a
/// collection, and it is written once for both rows so the two cannot come to
/// disagree about their shape. `pattern_to_type` recurses, so nothing else is
/// needed to instantiate it.
fn vec_of_vec_of_t() -> TypePattern {
    TypePattern::Collection {
        ctor: CollectionCtor::Vec,
        args: vec![vec_of_t()],
    }
}

/// `chunks(n)` — these elements in consecutive runs of `n`, the last short if
/// the length does not divide (ADR-149).
///
/// **No capability bound, for `reversed`'s reason**: a grouping reads no
/// descriptor callback — not `compare`, not `equals`, not `hash` — so a `Vec` of
/// closures chunks. What it *does* have that `reversed` has not is a fault, and
/// the fault is on the argument rather than on an element: `n <= 0` is refused
/// with `InvalidSize` before the receiver is walked. A run of zero elements is
/// not a short run, it is not a run — chunking any non-empty sequence into them
/// has no finite answer — and a negative one names nothing at all. That is the
/// row's whole faulting surface, which is why its manifest row is
/// `AllocatesAndFaults` and `reversed`'s is `Allocates`.
///
/// A *short last chunk* is not that fault and must not be confused with it:
/// `[1, 2, 3].chunks(2)` is `[[1, 2], [3]]`, because the question "which
/// consecutive runs of two are there" has an answer for a sequence of three and
/// the trailing element is part of it. `windows` is where a group that does not
/// fit is dropped instead, and the two differ there because they are asking
/// different questions.
fn seq_chunks() -> MethodEntry {
    MethodEntry {
        receiver: iterable_of_t(),
        name: "chunks",
        params: vec![TypePattern::Scalar(ScalarType::Int)],
        result: vec_of_vec_of_t(),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::VecChunks),
        doc: "Consecutive non-overlapping runs of n; the last may be shorter. \
              Faults if n is not positive.",
        stability: Stability::Stable,
    }
}

/// `windows(n)` — every consecutive run of exactly `n`, each starting one element
/// after the last (ADR-149).
///
/// Plural, like `chunks` beside it and like every other row that answers many
/// things — `frequencies`, `positions`, `cells`, `keys`, `items`. It is Rust's
/// spelling too, and it is what §6.3 and ADR-029 have called this row since
/// before it existed.
///
/// **A window that does not fit is dropped, and that is not the `chunks` fault
/// arriving late.** `[1, 2].windows(5)` is `[]`: "which runs of five are there"
/// is a perfectly good question about a sequence of two, and its answer is
/// none. What has no answer is a run of `n <= 0`, which is the same
/// `InvalidSize` [`seq_chunks`] raises and for the same reason.
fn seq_windows() -> MethodEntry {
    MethodEntry {
        receiver: iterable_of_t(),
        name: "windows",
        params: vec![TypePattern::Scalar(ScalarType::Int)],
        result: vec_of_vec_of_t(),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::VecWindows),
        doc: "Every consecutive run of exactly n, sliding by one. Empty if n \
              exceeds the length; faults if n is not positive.",
        stability: Stability::Stable,
    }
}

/// An iterable of `Text`s, spelled as a bounded variable for
/// [`iterable_of_int_elem`]'s reason: the row still *matches* a receiver whose
/// item is a `Char` and rejects it with `expected Text, found Char` at the
/// method name, rather than "no method `join` on this type".
fn iterable_of_text_elem() -> TypePattern {
    TypePattern::iterable(TypePattern::is_scalar("T", ScalarType::Text))
}

/// `join` — these `Text` elements concatenated with a separator between them
/// (ADR-144).
///
/// The separator is a required argument and not an optional one, because the
/// catalog has no optional arguments: a row is `(receiver, name, arity)`, and
/// `join()` beside `join(Text)` would be two rows for one question. `join("")`
/// is the no-separator spelling and says so where it is written.
///
/// **It renders nothing.** A `Vec[Int]` is a type error at the item, not a
/// sequence quietly stringified — which is what keeps `join` from being a back
/// door around ADR-143's decision about which types have a `to_text`. The
/// spelling is `ns.map(|n| n.to_text()).join(", ")`, and it says that it renders.
fn seq_join() -> MethodEntry {
    MethodEntry {
        receiver: iterable_of_text_elem(),
        name: "join",
        params: vec![TypePattern::Scalar(ScalarType::Text)],
        result: TypePattern::Scalar(ScalarType::Text),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::VecJoin),
        doc: "These Text items concatenated with `sep` between them.",
        stability: Stability::Stable,
    }
}

/// `frequencies` — a `Counter[T]` of how often each element occurs (§6.3, §6.2).
///
/// The **first** catalog row whose result is a keyed collection, and the reason
/// `Bound::Kind` exists. `require_collection_invariants` asks the key rule of a
/// method's receiver only; here the receiver is an ordinary `Vec` that is
/// allowed to hold anything, and it is the *result* that has keys. So the bound
/// is written on the row, where `MethodCatalogBuilder::finish` will refuse it if
/// it ever contradicts another.
fn seq_frequencies() -> MethodEntry {
    MethodEntry {
        receiver: TypePattern::iterable(TypePattern::of_kind("T", crate::CapKind::HashStable)),
        name: "frequencies",
        params: vec![],
        result: counter_of_t(),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::VecFrequencies),
        doc: "A Counter holding how many times each element occurs.",
        stability: Stability::Stable,
    }
}

// --- M8-WS11: the remaining non-barrier combinators (§6.3) ----------------
// Each is an intrinsic lowered by the MIR fuser (`recognize_pipeline` +
// `lower_pipeline`) into a single fused loop. Defined on both `Vec[T]` and
// `Seq[T]` so chains can start on a concrete collection and continue on Seq.

/// `(T, T) -> Bool` — the shape of `min_by`/`max_by`'s comparator ("less-than").
fn t_t_to_bool() -> TypePattern {
    TypePattern::Function {
        params: vec![TypePattern::var("T"), TypePattern::var("T")],
        result: Box::new(TypePattern::Scalar(ScalarType::Bool)),
    }
}

/// `(T) -> Vec<U>` — the shape of `flat_map`'s closure.
fn t_to_vec_u() -> TypePattern {
    TypePattern::Function {
        params: vec![TypePattern::var("T")],
        result: Box::new(vec_of_u()),
    }
}

// Streaming stages ---------------------------------------------------------

fn seq_take() -> MethodEntry {
    MethodEntry {
        receiver: iterable_of_t(),
        name: "take",
        params: vec![TypePattern::Scalar(ScalarType::Int)],
        result: vec_of_t(),
        purity: Purity::Pure,
        lowering: MethodLowering::Intrinsic("seq_take"),
        doc: "Keep at most the first n elements.",
        stability: Stability::Stable,
    }
}

fn seq_skip() -> MethodEntry {
    MethodEntry {
        receiver: iterable_of_t(),
        name: "skip",
        params: vec![TypePattern::Scalar(ScalarType::Int)],
        result: vec_of_t(),
        purity: Purity::Pure,
        lowering: MethodLowering::Intrinsic("seq_skip"),
        doc: "Drop the first n elements.",
        stability: Stability::Stable,
    }
}

fn seq_take_while() -> MethodEntry {
    MethodEntry {
        receiver: iterable_of_t(),
        name: "take_while",
        params: vec![t_to_bool()],
        result: vec_of_t(),
        purity: Purity::Pure,
        lowering: MethodLowering::Intrinsic("seq_take_while"),
        doc: "Keep elements until the predicate is false.",
        stability: Stability::Stable,
    }
}

/// `Vec[(Int, T)]` — what `enumerate` actually yields.
///
/// The row used to declare `Vec[T]`, the receiver's own element type, so
/// `v.enumerate()` on a `Vec[Int]` came out `Vec[Int]` and the tuple the fused
/// loop really builds was invisible to the type system. Found by S15 and
/// recorded there as a finding the register does not have; this stage is the one
/// that touches the sequence rows, so it is fixed here.
fn vec_of_index_and_t() -> TypePattern {
    TypePattern::Collection {
        ctor: CollectionCtor::Vec,
        args: vec![TypePattern::Tuple(vec![
            TypePattern::Scalar(ScalarType::Int),
            TypePattern::var("T"),
        ])],
    }
}

/// `Vec[(T, U)]` — what `zip` yields, pairing the receiver's element with the
/// argument sequence's.
///
/// The row used to declare both the parameter and the result as `Vec[T]`, which
/// said two wrong things at once: that the other sequence must have the *same*
/// element type as the receiver, and that the result is a sequence of that type
/// rather than of pairs.
fn vec_of_t_and_u() -> TypePattern {
    TypePattern::Collection {
        ctor: CollectionCtor::Vec,
        args: vec![TypePattern::Tuple(vec![
            TypePattern::var("T"),
            TypePattern::var("U"),
        ])],
    }
}

fn seq_enumerate() -> MethodEntry {
    MethodEntry {
        receiver: iterable_of_t(),
        name: "enumerate",
        params: vec![],
        result: vec_of_index_and_t(),
        purity: Purity::Pure,
        lowering: MethodLowering::Intrinsic("seq_enumerate"),
        doc: "Pair each element with its index.",
        stability: Stability::Stable,
    }
}

fn seq_zip() -> MethodEntry {
    MethodEntry {
        receiver: iterable_of_t(),
        name: "zip",
        params: vec![vec_of_u()],
        result: vec_of_t_and_u(),
        purity: Purity::Pure,
        lowering: MethodLowering::Intrinsic("seq_zip"),
        doc: "Pair elements with another sequence, stopping at the shorter length.",
        stability: Stability::Stable,
    }
}

fn seq_flat_map() -> MethodEntry {
    MethodEntry {
        receiver: iterable_of_t(),
        name: "flat_map",
        params: vec![t_to_vec_u()],
        result: vec_of_u(),
        purity: Purity::Pure,
        lowering: MethodLowering::Intrinsic("seq_flat_map"),
        doc: "Map each element to a Vec and concatenate the results.",
        stability: Stability::Stable,
    }
}

fn seq_filter_map() -> MethodEntry {
    MethodEntry {
        receiver: iterable_of_t(),
        name: "filter_map",
        params: vec![t_to_option_u()],
        result: vec_of_u(),
        purity: Purity::Pure,
        lowering: MethodLowering::Intrinsic("seq_filter_map"),
        doc: "Map each element to an Option and keep the Some payloads.",
        stability: Stability::Stable,
    }
}

// Aggregating sinks (scalar result) ---------------------------------------
//
// `sum`, `product`, `min` and `max` are **Int** operations (TY-31). Each one
// lowers to an `ExtractScalar` at `ScalarKind::Int` followed by an `IntBinOp` or
// an `IntCmp`, and the row's own result says `Int`. The element bound therefore
// has to be `Int` and not `Numeric`: a `Numeric` bound would bless `Float`, and
// `Vec[Float].sum()` reinterprets each float's bits as an integer and returns
// nonsense — the P0-12 class of bug, in a sink. `Bool` was accepted for the same
// reason, which is the finding.
//
// The bound is discharged by unification, so an element type that is *not yet
// known* is pinned to `Int` rather than merely allowed: `v.map(f).sum()` pins the
// closure's result.

/// An iterable of `Int`s, spelled as a bounded variable so the entry still
/// *matches* a receiver whose item is `Bool` or `Float` and rejects it with
/// `expected Int, found …` instead of "no method `sum` on this type".
///
/// Written once for all ten receivers rather than once per receiver, which is
/// what keeps the bound from drifting between them (ADR-127 decision 1).
fn iterable_of_int_elem() -> TypePattern {
    TypePattern::iterable(TypePattern::is_scalar("T", ScalarType::Int))
}

fn seq_product() -> MethodEntry {
    MethodEntry {
        receiver: iterable_of_int_elem(),
        name: "product",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        lowering: MethodLowering::Intrinsic("seq_product"),
        doc: "Multiply the (Int) elements.",
        stability: Stability::Stable,
    }
}

fn seq_min() -> MethodEntry {
    MethodEntry {
        receiver: iterable_of_int_elem(),
        name: "min",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        lowering: MethodLowering::Intrinsic("seq_min"),
        doc: "Smallest (Int) element. Faults on an empty sequence (D1).",
        stability: Stability::Stable,
    }
}

fn seq_max() -> MethodEntry {
    MethodEntry {
        receiver: iterable_of_int_elem(),
        name: "max",
        params: vec![],
        result: TypePattern::Scalar(ScalarType::Int),
        purity: Purity::Pure,
        lowering: MethodLowering::Intrinsic("seq_max"),
        doc: "Largest (Int) element. Faults on an empty sequence (D1).",
        stability: Stability::Stable,
    }
}

fn seq_min_by() -> MethodEntry {
    MethodEntry {
        receiver: iterable_of_t(),
        name: "min_by",
        params: vec![t_t_to_bool()],
        result: TypePattern::var("T"),
        purity: Purity::Pure,
        lowering: MethodLowering::Intrinsic("seq_min_by"),
        doc: "Smallest element per a (T,T)->Bool \"less-than\" comparator.",
        stability: Stability::Stable,
    }
}

fn seq_max_by() -> MethodEntry {
    MethodEntry {
        receiver: iterable_of_t(),
        name: "max_by",
        params: vec![t_t_to_bool()],
        result: TypePattern::var("T"),
        purity: Purity::Pure,
        lowering: MethodLowering::Intrinsic("seq_max_by"),
        doc: "Largest element per a (T,T)->Bool \"less-than\" comparator.",
        stability: Stability::Stable,
    }
}

fn seq_any() -> MethodEntry {
    MethodEntry {
        receiver: iterable_of_t(),
        name: "any",
        params: vec![t_to_bool()],
        result: TypePattern::Scalar(ScalarType::Bool),
        purity: Purity::Pure,
        lowering: MethodLowering::Intrinsic("seq_any"),
        doc: "True if any element satisfies the predicate (short-circuits).",
        stability: Stability::Stable,
    }
}

fn seq_all() -> MethodEntry {
    MethodEntry {
        receiver: iterable_of_t(),
        name: "all",
        params: vec![t_to_bool()],
        result: TypePattern::Scalar(ScalarType::Bool),
        purity: Purity::Pure,
        lowering: MethodLowering::Intrinsic("seq_all"),
        doc: "True if all elements satisfy the predicate (short-circuits).",
        stability: Stability::Stable,
    }
}

fn seq_find() -> MethodEntry {
    MethodEntry {
        receiver: iterable_of_t(),
        name: "find",
        params: vec![t_to_bool()],
        result: TypePattern::Option(Box::new(TypePattern::var("T"))),
        purity: Purity::Pure,
        lowering: MethodLowering::Intrinsic("seq_find"),
        doc: "The first matching element, or None.",
        stability: Stability::Stable,
    }
}

fn seq_position() -> MethodEntry {
    MethodEntry {
        receiver: iterable_of_t(),
        name: "position",
        params: vec![t_to_bool()],
        result: TypePattern::Option(Box::new(TypePattern::Scalar(ScalarType::Int))),
        purity: Purity::Pure,
        lowering: MethodLowering::Intrinsic("seq_position"),
        doc: "The index of the first matching element, or None.",
        stability: Stability::Stable,
    }
}

fn seq_reduce() -> MethodEntry {
    MethodEntry {
        receiver: iterable_of_t(),
        name: "reduce",
        params: vec![t_t_to_t()],
        result: TypePattern::var("T"),
        purity: Purity::Pure,
        lowering: MethodLowering::Intrinsic("seq_reduce"),
        doc: "Reduce left-to-right, seeded with the first element.",
        stability: Stability::Stable,
    }
}

/// `sorted_by_key(f)` — a new `Vec` ordered by the key `f` extracts (ADR-127
/// decision 5).
///
/// **A keyed collection cannot order its own items.** ADR-045 decided that no
/// composite is orderable, because MIR had one integer compare and `(1, 2) < (1,
/// 3)` would have compared two schema pointers — so the moment a pipeline's item
/// is a pair, which is the moment its source is a `Map` or a `Counter`, `sorted`
/// is unavailable and "the five most common values" has no spelling.
///
/// So the `Ord` bound is on the **extracted key** rather than on the element,
/// and the composite-ordering question ADR-045 deferred stays deferred.
///
/// A barrier like `sorted`, and for the same reason: it needs the whole sequence
/// before it can answer its first element. Its receiver is `Iterable`, so
/// `build::emit_iter_vec` materializes it in front of the wrapper.
fn seq_sorted_by_key() -> MethodEntry {
    MethodEntry {
        receiver: iterable_of_t(),
        name: "sorted_by_key",
        params: vec![TypePattern::Function {
            params: vec![TypePattern::var("T")],
            result: Box::new(TypePattern::of_kind("K", crate::CapKind::Ord)),
        }],
        result: vec_of_t(),
        purity: Purity::Pure,
        lowering: MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::VecSortedByKey),
        doc: "A new Vec ordered by the key the closure extracts.",
        stability: Stability::Stable,
    }
}

// --- the conversions (ADR-127 decision 4) ---------------------------------
//
// **A pipeline's currency is `Vec`**: every streaming stage answers one,
// whatever the receiver was, and a program that wants a collection back says
// which one. `set.filter(p)` is a `Vec[T]`; `set.filter(p).to_set()` is a
// `Set[T]`. One sentence answers "what does `filter` return" for all ten
// receivers, and it is answerable without knowing which receiver you are on —
// which is what decision 6 declined `map_values` and the shape-preserving family
// to keep.
//
// Each is a **fused sink**, not a barrier: `Sink::CollectInto`'s accumulator is
// the target collection, so `v.map(f).to_set()` is one loop with no intermediate
// `Vec`. The per-element step is one wrapper that already exists.
//
// **The set is closed at "every collection with a constructor", and that is the
// point.** The ask named `Set`/`Map`/`Counter`; the last four are here because
// leaving them out is what creates an asymmetry decision 6 would then have to
// defend — `deque.filter(p)` answering a `Vec` with no way back to a `Deque`,
// and no way to build a heap from a sequence without a `while` loop. `Grid` is
// the one collection with no row: a grid needs a width, and a flat item sequence
// does not carry one.

/// `to_vec()` — the item sequence as a `Vec`.
///
/// **This is not `collect` coming back.** ADR-126 deleted `collect` because it
/// "named a step the compiler takes anyway" — a chain ending on a stage already
/// materializes. For nine of `to_vec`'s ten receivers it names a step the
/// compiler does **not** take: `s.to_vec()` is the only way to get a `Vec[T]` out
/// of a `Set`, and `m.to_vec()` the only way to get `Vec[(K, V)]` out of a `Map`,
/// where `keys()` and `values()` answer two aligned halves and nothing joins
/// them.
///
/// On a `Vec` receiver it degenerates to the identity, and it answers **the same
/// reference**, not a copy. That is the second half of ADR-126 decision 2 kept:
/// that decision declined to leave a shallow copy behind "under a name that does
/// not mention it", and this name does not mention one either.
fn seq_to_vec() -> MethodEntry {
    MethodEntry {
        receiver: iterable_of_t(),
        name: "to_vec",
        params: vec![],
        result: vec_of_t(),
        purity: Purity::Pure,
        lowering: MethodLowering::Intrinsic("seq_to_vec"),
        doc: "The items as a Vec. On a Vec receiver this is the receiver itself.",
        stability: Stability::Stable,
    }
}

/// `to_set()` — a `Set[T]`, duplicates dropped and no order kept.
///
/// `HashStable` for the reason every key rule is: an element that can change
/// after it is stored moves its own bucket without moving the entry, and cannot
/// be found again (D4, TY-32). The bound is the row's own because
/// `require_collection_invariants` asks about the *receiver*, and here it is the
/// **result** that has the keys — the same shape `frequencies` established.
///
/// Not the same question as `unique()`, and the reason is worth writing next to
/// both: `unique` answers a `Vec` in first-occurrence order, and a `Set` has no
/// order to preserve.
fn seq_to_set() -> MethodEntry {
    MethodEntry {
        receiver: TypePattern::iterable(TypePattern::of_kind("T", crate::CapKind::HashStable)),
        name: "to_set",
        params: vec![],
        result: set_of_t(),
        purity: Purity::Pure,
        lowering: MethodLowering::Intrinsic("seq_to_set"),
        doc: "A Set holding these items, duplicates dropped.",
        stability: Stability::Stable,
    }
}

/// `to_map()` — a `Map[K, V]`, on a pipeline whose item is a pair.
///
/// **The receiver pattern says "a pair" rather than prose saying it**, so
/// `[1, 2].to_map()` is a unification failure at the method name — "expected
/// `(K, V)`, found `Int`" — and not a row that resolves and then faults. That is
/// the whole of why `TypePattern::Iterable` carries an item pattern at all.
///
/// Duplicate keys resolve last-wins, which is `insert`'s existing rule.
fn seq_to_map() -> MethodEntry {
    MethodEntry {
        receiver: TypePattern::iterable(TypePattern::Tuple(vec![
            TypePattern::of_kind("K", crate::CapKind::HashStable),
            TypePattern::var("V"),
        ])),
        name: "to_map",
        params: vec![],
        result: map_of_k_v(),
        purity: Purity::Pure,
        lowering: MethodLowering::Intrinsic("seq_to_map"),
        doc: "A Map built from (key, value) pairs. Duplicate keys: last wins.",
        stability: Stability::Stable,
    }
}

/// `to_counter()` — a `Counter[T]` from `(T, Int)` pairs, taking each pair's
/// count.
///
/// `frequencies()` is the other direction and neither expresses the other:
/// `v.frequencies()` *counts* occurrences of each element, `pairs.to_counter()`
/// *assigns* the count each pair carries. They are the two directions of one type
/// change, so both stay.
fn seq_to_counter() -> MethodEntry {
    MethodEntry {
        receiver: TypePattern::iterable(TypePattern::Tuple(vec![
            TypePattern::of_kind("T", crate::CapKind::HashStable),
            TypePattern::Scalar(ScalarType::Int),
        ])),
        name: "to_counter",
        params: vec![],
        result: counter_of_t(),
        purity: Purity::Pure,
        lowering: MethodLowering::Intrinsic("seq_to_counter"),
        doc: "A Counter built from (key, count) pairs. Duplicate keys: last wins.",
        stability: Stability::Stable,
    }
}

fn seq_to_deque() -> MethodEntry {
    MethodEntry {
        receiver: iterable_of_t(),
        name: "to_deque",
        params: vec![],
        result: deque_of_t(),
        purity: Purity::Pure,
        lowering: MethodLowering::Intrinsic("seq_to_deque"),
        doc: "A Deque holding these items, in order.",
        stability: Stability::Stable,
    }
}

/// `to_min_heap()` — and [`seq_to_max_heap`], its dual.
///
/// The `Ord` bound is on the row for `to_set`'s reason: the heap being built is
/// the *result*, and a heap orders its elements as it pushes them.
fn seq_to_min_heap() -> MethodEntry {
    MethodEntry {
        receiver: TypePattern::iterable(TypePattern::of_kind("T", crate::CapKind::Ord)),
        name: "to_min_heap",
        params: vec![],
        result: min_heap_of_t(),
        purity: Purity::Pure,
        lowering: MethodLowering::Intrinsic("seq_to_min_heap"),
        doc: "A MinHeap holding these items.",
        stability: Stability::Stable,
    }
}

fn seq_to_max_heap() -> MethodEntry {
    MethodEntry {
        receiver: TypePattern::iterable(TypePattern::of_kind("T", crate::CapKind::Ord)),
        name: "to_max_heap",
        params: vec![],
        result: max_heap_of_t(),
        purity: Purity::Pure,
        lowering: MethodLowering::Intrinsic("seq_to_max_heap"),
        doc: "A MaxHeap holding these items.",
        stability: Stability::Stable,
    }
}

/// `to_bitset()` — a `BitSet` of `Int` members.
///
/// The receiver says `Int` the way `to_map` says "a pair", so a non-`Int` item is
/// a type error at the method name. `praxis_bitset_insert` still faults on a
/// negative or oversized member, which is a *value* question no type can answer.
fn seq_to_bitset() -> MethodEntry {
    MethodEntry {
        receiver: iterable_of_int_elem(),
        name: "to_bitset",
        params: vec![],
        result: bitset_receiver(),
        purity: Purity::Pure,
        lowering: MethodLowering::Intrinsic("seq_to_bitset"),
        doc: "A BitSet holding these (Int) items. Faults on a negative or oversized member.",
        stability: Stability::Stable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Every catalog row describes itself.** The `doc` field is what hover,
    /// completion and signature help put in front of a reader, so a row added
    /// with an empty or placeholder one ships a method the editor can name and
    /// cannot explain — and nothing else in the build would notice, because the
    /// field is a `&'static str` that `""` satisfies.
    ///
    /// The sentence rule is the half that catches a placeholder: `"TODO"` and
    /// `"len"` are both long enough to pass a length floor alone.
    #[test]
    fn every_catalog_row_documents_itself() {
        for e in builtin_catalog().entries() {
            let what = format!("{}.{}/{}", e.receiver, e.name, e.arity());
            assert!(e.doc.len() > 8, "{what} has no real documentation");
            assert!(
                e.doc.ends_with('.') || e.doc.ends_with(')'),
                "{what}'s doc is not a sentence: {:?}",
                e.doc
            );
            // A backtick opens a sentence too: the subscript rows lead with the
            // syntax they are about (`` `v[i]` — the element at `i` ``), which
            // is the clearest thing they could say and not a placeholder.
            assert!(
                e.doc
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_uppercase() || c == '`'),
                "{what}'s doc does not open a sentence: {:?}",
                e.doc
            );
        }
    }

    #[test]
    fn builtin_catalog_has_vec_methods() {
        let cat = builtin_catalog();
        assert!(cat.len() >= 4);
        let vec_pat = vec_of_t();
        let push_hits: Vec<_> = cat.by_receiver_and_name(&vec_pat, "push").collect();
        assert_eq!(push_hits.len(), 1);
        assert_eq!(
            push_hits[0].lowering,
            MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::VecPush)
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
        assert!(get.can_fault());
        // Derived, not restated — and the derivation corrects a row that had
        // drifted: `bitset.insert` declared `can_fault: false` while
        // `praxis_bitset_insert` raises `InvalidSize` for a member outside
        // `BitIndex`'s range.
        let bitset_pat = bitset_receiver();
        let insert = cat
            .by_receiver_and_name(&bitset_pat, "insert")
            .next()
            .expect("bitset.insert exists");
        assert!(insert.can_fault());
    }

    /// **REP-18.** A keyed collection can be enumerated, `count` has two arities,
    /// and every enumeration answers a `Vec` so §6.3 applies to it.
    ///
    /// §3.3's last line is `counts.values().count(|n| n >= 2)` and **neither half
    /// existed**: `values` was in no row, and `count` only at arity zero. The
    /// second arity is not a language decision — the catalog's key has always been
    /// `(receiver, name, arity)` — but it is the first row to use it, so this is
    /// where that is written down.
    #[test]
    fn a_keyed_collection_enumerates_and_count_has_two_arities() {
        let cat = builtin_catalog();
        let map_pat = TypePattern::Collection {
            ctor: CollectionCtor::Map,
            args: vec![TypePattern::var("K"), TypePattern::var("V")],
        };
        let counter_pat = counter_of_t();

        // Both collections enumerate both ways, and each answers a `Vec`.
        for (pat, what) in [(map_pat.clone(), "Map"), (counter_pat.clone(), "Counter")] {
            for name in ["keys", "values"] {
                let hits: Vec<_> = cat.by_receiver_and_name(&pat, name).collect();
                assert_eq!(hits.len(), 1, "{what}.{name}()");
                assert_eq!(hits[0].arity(), 0, "{what}.{name}() takes no arguments");
                assert!(
                    matches!(
                        hits[0].result,
                        TypePattern::Collection {
                            ctor: CollectionCtor::Vec,
                            ..
                        }
                    ),
                    "{what}.{name}() answers a Vec so every §6.3 combinator applies"
                );
            }
        }

        // A `Counter`'s values are its counts, whatever its key type is (§6.2),
        // where a `Map`'s are its value type. The two are not the same row written
        // twice.
        let counter_values = cat
            .by_receiver_and_name(&counter_pat, "values")
            .next()
            .expect("Counter.values");
        assert_eq!(
            counter_values.result,
            TypePattern::Collection {
                ctor: CollectionCtor::Vec,
                args: vec![TypePattern::Scalar(ScalarType::Int)]
            }
        );
        let map_values = cat
            .by_receiver_and_name(&map_pat, "values")
            .next()
            .expect("Map.values");
        assert_eq!(
            map_values.result,
            TypePattern::Collection {
                ctor: CollectionCtor::Vec,
                args: vec![TypePattern::var("V")]
            }
        );
        // …and `keys()` is the *key* type, which is what makes `m[ks[i]]` legal.
        let map_keys = cat
            .by_receiver_and_name(&map_pat, "keys")
            .next()
            .expect("Map.keys");
        assert_eq!(
            map_keys.result,
            TypePattern::Collection {
                ctor: CollectionCtor::Vec,
                args: vec![TypePattern::var("K")]
            }
        );

        // `count` at two arities — one pair of rows now, on the generic receiver
        // every pipeline starts from (ADR-127).
        let arities: Vec<usize> = cat
            .by_receiver_and_name(&iterable_of_t(), "count")
            .map(|e| e.arity())
            .collect();
        assert_eq!(arities.len(), 2, "count has two rows");
        assert!(arities.contains(&0) && arities.contains(&1));
    }

    /// **ADR-127 decision 1.** Every §6.3 combinator is **one** row, on the
    /// generic pipeline receiver.
    ///
    /// The shape this replaces was forty-nine rows: a `Vec[T]` row and a `Seq[T]`
    /// twin apiece, of which every `Seq` twin was dead — ADR-126 recorded that
    /// "no catalog row answers a `Seq` … so all twenty `Seq`-receiver rows are
    /// dead", and nothing answered one in three milestones. So the assertion is
    /// two-sided: each name is on `Iterable` and on nothing else, and no
    /// `Seq`-receiver row is left anywhere in the table.
    #[test]
    fn every_pipeline_combinator_is_one_row_on_the_generic_receiver() {
        let cat = builtin_catalog();
        // The twenty-three fused stages and sinks, the six barriers, and the
        // eight conversions. `count` is here once and checked at both arities
        // by `a_keyed_collection_enumerates_and_count_has_two_arities`.
        //
        // `to_text` is deliberately *not* in this list: it has no generic row,
        // and the reason is written on `vec_to_text` (ADR-144).
        let combinators = [
            "map",
            "filter",
            "filter_map",
            "flat_map",
            "take",
            "skip",
            "take_while",
            "enumerate",
            "zip",
            "fold",
            "reduce",
            "sum",
            "product",
            "count",
            "any",
            "all",
            "find",
            "position",
            "min",
            "max",
            "min_by",
            "max_by",
            "sorted",
            "sorted_by_key",
            "unique",
            "reversed",
            "frequencies",
            "join",
            "to_vec",
            "to_set",
            "to_map",
            "to_counter",
            "to_deque",
            "to_min_heap",
            "to_max_heap",
            "to_bitset",
        ];
        for name in combinators {
            let rows: Vec<_> = cat.entries().iter().filter(|e| e.name == name).collect();
            assert!(!rows.is_empty(), "`{name}` has no row at all");
            // One row per *arity*: `count()` is the element count and
            // `count(pred)` the matching-element count, which the catalog's key
            // has always allowed. Two at one arity would be the duplication this
            // decision deleted.
            let mut generic: Vec<usize> = rows
                .iter()
                .filter(|e| matches!(e.receiver, TypePattern::Iterable { .. }))
                .map(|e| e.arity())
                .collect();
            assert!(!generic.is_empty(), "`{name}` has no generic row");
            let seen = generic.len();
            generic.sort_unstable();
            generic.dedup();
            assert_eq!(
                generic.len(),
                seen,
                "`{name}` has two generic rows at one arity — one receiver \
                 getting a feature ten should have"
            );
            // A row that shares the *name* must be on a receiver the generic one
            // does not accept, which the builder's collision check enforces and
            // this restates from the reader's side. `Grid[T].find(value)` is the
            // live example: §6.4's "where is this cell" is a different question
            // from §6.3's "which element matches", and they can coexist because a
            // `Grid` is not one of the ten.
            for row in rows {
                assert!(
                    matches!(row.receiver, TypePattern::Iterable { .. })
                        || !crate::is_pipeline_receiver(&row.receiver),
                    "`{name}` also has a row on {}, which the generic row \
                     accepts — both would match, and which one a call resolves \
                     to would be insertion order",
                    row.receiver
                );
            }
        }

        // `Grid[T].map` is the row §6.4 still owes, and the *absence* of a
        // generic row claiming the name is what leaves room for it. The builder's
        // collision check is the other half; this is the half that says the
        // exclusion was deliberate.
        assert!(
            !crate::PIPELINE_RECEIVERS.contains(&CollectionCtor::Grid),
            "a generic `map` would claim §6.4's name and answer a `Vec`"
        );

        // Nothing is registered on a `Seq` any more, which is what makes
        // retiring `CollectionCtor::Seq` a mechanical follow-up.
        for e in cat.entries() {
            assert!(
                !matches!(
                    e.receiver,
                    TypePattern::Collection {
                        ctor: CollectionCtor::Seq,
                        ..
                    }
                ),
                "`{}` is still on a `Seq`, which has no values",
                e.name
            );
        }
    }

    /// **ADR-127 decision 4.** There is a conversion for every collection with a
    /// constructor, and exactly one collection with none.
    ///
    /// The set is closed at that boundary on purpose: the ask named
    /// `Set`/`Map`/`Counter`, and leaving the other four out is what would create
    /// the asymmetry decision 6 then has to defend — `deque.filter(p)` answering
    /// a `Vec` with no way back to a `Deque`. `Grid` is the exception because a
    /// grid needs a width and a flat item sequence does not carry one.
    #[test]
    fn a_conversion_exists_for_every_collection_that_can_be_constructed() {
        let cat = builtin_catalog();
        for (name, ctor) in [
            ("to_vec", CollectionCtor::Vec),
            ("to_set", CollectionCtor::Set),
            ("to_map", CollectionCtor::Map),
            ("to_counter", CollectionCtor::Counter),
            ("to_deque", CollectionCtor::Deque),
            ("to_min_heap", CollectionCtor::MinHeap),
            ("to_max_heap", CollectionCtor::MaxHeap),
            ("to_bitset", CollectionCtor::BitSet),
        ] {
            let row = cat
                .entries()
                .iter()
                .find(|e| e.name == name)
                .unwrap_or_else(|| panic!("`{name}` has no row"));
            assert_eq!(row.arity(), 0, "`{name}` takes no arguments");
            let built = match &row.result {
                TypePattern::Collection { ctor, .. } => *ctor,
                other => panic!("`{name}` answers {other}, not a collection"),
            };
            assert_eq!(built, ctor, "`{name}` builds the collection it names");
        }
        assert!(
            cat.entries().iter().all(|e| e.name != "to_grid"),
            "a grid needs a width, and an item sequence does not carry one"
        );

        // **The pair shape is in the receiver, not in prose.** That is what makes
        // `[1, 2].to_map()` "expected `(K, V)`, found `Int`" at the method name
        // rather than a row that resolves and then faults at runtime.
        for name in ["to_map", "to_counter"] {
            let row = cat.entries().iter().find(|e| e.name == name).unwrap();
            let TypePattern::Iterable { item } = &row.receiver else {
                panic!("`{name}` is not on the generic receiver")
            };
            assert!(
                matches!(**item, TypePattern::Tuple(ref els) if els.len() == 2),
                "`{name}` accepts a `Map` or a `Counter` by saying its item is a pair"
            );
        }
    }

    /// **REP-16.** The subscript rows are the closed set the language documents,
    /// and their names cannot be written in source.
    ///
    /// Two properties in one test because they are both about the same decision.
    /// Which collections index is a *language* answer (§4.7/§6.2/§6.4), so a row
    /// added or dropped by accident should fail here rather than surface as a
    /// program that mysteriously compiles. And a row whose name is an identifier
    /// would be callable as `m.foo(k)`, which no design section describes.
    #[test]
    fn the_subscript_rows_are_a_closed_set_no_program_can_name() {
        let cat = builtin_catalog();
        let of = |ctor: CollectionCtor, args: usize| TypePattern::Collection {
            ctor,
            args: (0..args).map(|_| TypePattern::var("T")).collect(),
        };
        let map_pat = TypePattern::Collection {
            ctor: CollectionCtor::Map,
            args: vec![TypePattern::var("K"), TypePattern::var("V")],
        };

        // Six read. `Text` is a scalar receiver, so it is spelled differently.
        for (pat, indices, what) in [
            (of(CollectionCtor::Vec, 1), 1, "Vec"),
            (of(CollectionCtor::Deque, 1), 1, "Deque"),
            (map_pat.clone(), 1, "Map"),
            (of(CollectionCtor::Counter, 1), 1, "Counter"),
            (of(CollectionCtor::Grid, 1), 2, "Grid"),
            (text_receiver(), 1, "Text"),
        ] {
            let hits: Vec<_> = cat
                .by_receiver_and_name(&pat, crate::catalog::INDEX_READ)
                .collect();
            assert_eq!(hits.len(), 1, "{what} reads through exactly one row");
            assert_eq!(hits[0].arity(), indices, "{what} indexes at {indices}");
        }

        // Five store — every reader but `Text`. The asymmetry the `Y020` message
        // has to describe is now `Text`'s alone: it reads a `Char` out and is
        // immutable (§4.3), so there is nothing to write back through.
        for (pat, args, what) in [
            (of(CollectionCtor::Vec, 1), 2, "Vec"),
            (of(CollectionCtor::Deque, 1), 2, "Deque"),
            (map_pat.clone(), 2, "Map"),
            (of(CollectionCtor::Counter, 1), 2, "Counter"),
            (of(CollectionCtor::Grid, 1), 3, "Grid"),
        ] {
            let hits: Vec<_> = cat
                .by_receiver_and_name(&pat, crate::catalog::INDEX_STORE)
                .collect();
            assert_eq!(hits.len(), 1, "{what} stores through exactly one row");
            assert_eq!(
                hits[0].arity(),
                args,
                "{what}'s store takes its indices and then the value"
            );
        }

        // Nothing else has either row.
        for (pat, what) in [
            (of(CollectionCtor::Set, 1), "Set"),
            (of(CollectionCtor::MinHeap, 1), "MinHeap"),
            (of(CollectionCtor::MaxHeap, 1), "MaxHeap"),
            (of(CollectionCtor::BitSet, 0), "BitSet"),
        ] {
            for name in [crate::catalog::INDEX_READ, crate::catalog::INDEX_STORE] {
                assert_eq!(
                    cat.by_receiver_and_name(&pat, name).count(),
                    0,
                    "{what} has no `{name}`"
                );
            }
        }
        assert_eq!(
            cat.by_receiver_and_name(&text_receiver(), crate::catalog::INDEX_STORE)
                .count(),
            0,
            "a `Text` is immutable (§4.3): it reads through a subscript and has \
             no element store"
        );

        // A `Vec`/`Deque` store is a **replacement**, and the wrapper it names is
        // the assertion of that: `praxis_vec_set` faults on an index the vector
        // does not hold, where `praxis_vec_push` would have grown it. A row that
        // pointed at the push would spell `v[i] = x` and mean `v.push(x)`.
        for (pat, push, what) in [
            (of(CollectionCtor::Vec, 1), "push", "Vec"),
            (of(CollectionCtor::Deque, 1), "push_back", "Deque"),
        ] {
            let store = cat
                .by_receiver_and_name(&pat, crate::catalog::INDEX_STORE)
                .next()
                .unwrap_or_else(|| panic!("{what}'s store"));
            let appender = cat
                .by_receiver_and_name(&pat, push)
                .next()
                .unwrap_or_else(|| panic!("{what}.{push}"));
            assert_ne!(
                store.lowering, appender.lowering,
                "{what}'s store replaces; `{push}` appends"
            );
            assert!(
                store.can_fault(),
                "{what}'s store reports an index it does not hold"
            );
        }

        // A `Map`'s two reads are two *different* wrappers: §4.7 gives `.get` and
        // `map[key]` different answers about an absent key, so pointing both rows
        // at one wrapper would take the choice away from the user.
        let get = cat
            .by_receiver_and_name(&map_pat, "get")
            .next()
            .expect("Map.get");
        let index = cat
            .by_receiver_and_name(&map_pat, crate::catalog::INDEX_READ)
            .next()
            .expect("Map's subscript");
        assert_ne!(get.lowering, index.lowering);
        assert!(
            index.can_fault() && !get.can_fault(),
            "indexing faults where `.get` answers"
        );

        // The two **updating** stores (REP-21): `Map` only, at the same arity as
        // its plain store, and pointing at wrappers of their own — a row that
        // reused `MapInsert` would spell `min=` and mean `=`.
        let map_int_value = TypePattern::Collection {
            ctor: CollectionCtor::Map,
            args: vec![
                TypePattern::var("K"),
                TypePattern::is_scalar("V", ScalarType::Int),
            ],
        };
        let plain_store = cat
            .by_receiver_and_name(&map_pat, crate::catalog::INDEX_STORE)
            .next()
            .expect("Map's store")
            .lowering
            .clone();
        for (name, what) in [
            (crate::catalog::INDEX_STORE_MIN, "min="),
            (crate::catalog::INDEX_STORE_MAX, "max="),
        ] {
            let hits: Vec<_> = cat.by_receiver_and_name(&map_int_value, name).collect();
            assert_eq!(hits.len(), 1, "`{what}` is one row on a Map");
            assert_eq!(hits[0].arity(), 2, "`{what}` takes its key and its value");
            assert_ne!(
                hits[0].lowering, plain_store,
                "`{what}` must not lower to the plain store"
            );
            // …and no other receiver has one, including the collections that do
            // have a plain store.
            for (pat, other) in [
                (of(CollectionCtor::Counter, 1), "Counter"),
                (of(CollectionCtor::Grid, 1), "Grid"),
                (of(CollectionCtor::Vec, 1), "Vec"),
                (of(CollectionCtor::Set, 1), "Set"),
            ] {
                assert_eq!(
                    cat.by_receiver_and_name(&pat, name).count(),
                    0,
                    "{other} has no `{what}`"
                );
            }
        }
        // The two are different rows from each other, or one of them computes
        // the other's answer.
        assert_ne!(
            cat.by_receiver_and_name(&map_int_value, crate::catalog::INDEX_STORE_MIN)
                .next()
                .expect("min=")
                .lowering,
            cat.by_receiver_and_name(&map_int_value, crate::catalog::INDEX_STORE_MAX)
                .next()
                .expect("max=")
                .lowering,
        );

        // No subscript name is an identifier, so the subscript grammar is their
        // only caller: the parser accepts only an `Ident` after `.`.
        for name in [
            crate::catalog::INDEX_READ,
            crate::catalog::INDEX_STORE,
            crate::catalog::INDEX_STORE_MIN,
            crate::catalog::INDEX_STORE_MAX,
        ] {
            assert!(
                !name
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphabetic() || c == '_'),
                "`{name}` must not be spellable as a method name"
            );
        }
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
                n => (0..n).map(|_| TypePattern::var("T")).collect(),
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
            args: vec![TypePattern::var("K"), TypePattern::var("V")],
        };
        assert!(cat.by_receiver_and_name(&map_pat, "len").count() >= 1);
        assert!(cat.by_receiver_and_name(&map_pat, "is_empty").count() >= 1);
        // Grid has width/height (its dimension methods).
        let grid_pat = TypePattern::Collection {
            ctor: CollectionCtor::Grid,
            args: vec![TypePattern::var("T")],
        };
        assert!(cat.by_receiver_and_name(&grid_pat, "width").count() >= 1);
        assert!(cat.by_receiver_and_name(&grid_pat, "neighbors4").count() >= 1);
    }

    /// **S18's standing invariant.** Every catalog row that lowers to a runtime
    /// wrapper declares a `Unit` result if and only if that wrapper's manifest
    /// return is `AbiRet::GcUnit`.
    ///
    /// This is RT-14 and RT-15 stated as a rule instead of as two bugs.
    /// `Map.get` declared `V` and `Grid.find` declared `(Int, Int)`; both were
    /// non-faulting; both answered `unit_sentinel(ctx)` when the key or the cell
    /// was not there. So a program held a value whose *static* type was `Int`
    /// and whose *runtime descriptor* was `Unit`, with nothing in the workspace
    /// able to notice — `allocates` and `can_fault` are derived from the
    /// manifest, but "can this answer be Unit" had nowhere to live at all.
    ///
    /// It has somewhere now, and the **absence of a third `AbiRet` arm** is the
    /// load-bearing part rather than this test. "May be Unit, may be a value" is
    /// exactly the defect, and it cannot be spelled: an author restoring RT-14
    /// has to write either `GcUnit` — which this test refuses beside a `V`
    /// result — or `Gc`, which is then a claim the wrapper must honour and which
    /// `absent_map_get_does_not_return_an_untyped_unit_sentinel` checks in the
    /// runtime. Between the two there is nowhere for the old behaviour to live.
    ///
    /// **What this does not prove**, stated plainly: the manifest row is
    /// hand-asserted, at exactly the trust level `Effect` already is, so what
    /// this catches is a *catalog row disagreeing with its manifest row*, not a
    /// manifest row that lies about its wrapper. Reverting `map_get`'s result to
    /// `TypePattern::var("V")` and leaving `MapGet` at `-> Gc` passes here, and
    /// is caught in `praxis-runtime` instead. The precedent for trusting a
    /// hand-written manifest row that far is `MethodEntry::can_fault`'s own doc:
    /// the per-row `bool` it replaced had drifted, and one place to write the
    /// fact is what fixed it.
    ///
    /// The sweep runs over *every* such row, faulting ones included. A faulting
    /// wrapper's Unit is the ABI's universal unwind answer, which is a different
    /// thing from its declared result — so the biconditional holds there too,
    /// and restricting the sweep would only make it weaker.
    #[test]
    fn a_non_faulting_row_with_a_value_result_cannot_answer_the_unit_sentinel() {
        let cat = builtin_catalog();
        let mut checked = 0;
        for entry in cat.entries() {
            // An intrinsic has no wrapper: it expands to MIR instructions whose
            // effects are their own (`MethodEntry::can_fault` says the same).
            let MethodLowering::RuntimeSymbol(sym) = entry.lowering else {
                continue;
            };
            checked += 1;
            let ret = sym.sig().ret;
            let result_is_unit = entry.result == TypePattern::Unit;
            match (ret, result_is_unit) {
                (abi::AbiRet::GcUnit, true) | (abi::AbiRet::Gc, false) => {}
                _ => panic!(
                    "{}.{} declares `{}` and lowers to `{}`, whose manifest return is \
                     {ret:?}. A wrapper answers either a value (`AbiRet::Gc`, \
                     non-`Unit` result) or nothing (`AbiRet::GcUnit`, `Unit` \
                     result); an answer that is sometimes absent is spelled \
                     `Option[T]` (§4.7), never a Unit sentinel under a value type.",
                    entry.receiver,
                    entry.name,
                    entry.result,
                    sym.name(),
                ),
            }
        }
        // A guard against the sweep silently covering nothing — every row being
        // faulting, or `entries()` being empty, would otherwise pass.
        assert!(
            checked >= 40,
            "expected the sweep to reach most of the catalog, it reached {checked} rows"
        );
    }

    /// The sweep above skips [`MethodLowering::ScalarPrimitive`] rows, because
    /// its biconditional is about wrappers that answer a `GcRef` and those
    /// answer the scalar channel. This is their half of it (ADR-118 decision 6).
    ///
    /// **Skipping a row is how a sweep quietly stops covering anything**, so
    /// the arm that made them skippable owes this test the day it is added.
    /// What it rules out is the state that would matter: a row marked
    /// `ScalarPrimitive` while its wrapper still returns a `GcRef`, which MIR
    /// would take at its word and put a raw integer into a rootable slot
    /// (P0-03). The complementary refusal is in `praxis-mir`'s
    /// `lower_scalar_primitive`, whose fallthrough is an ICE naming the symbol:
    /// a `-> RawI64` wrapper reachable from a method call with no instruction
    /// to produce it is a compiler bug, not a program's.
    #[test]
    fn a_scalar_primitive_row_answers_the_scalar_channel_and_a_scalar_type() {
        let cat = builtin_catalog();
        let mut rows = 0;
        for entry in cat.entries() {
            let MethodLowering::ScalarPrimitive(sym) = entry.lowering else {
                continue;
            };
            rows += 1;
            assert_eq!(
                sym.sig().ret,
                abi::AbiRet::RawI64,
                "{}.{} lowers as a scalar primitive, so `{}` must answer the \
                 scalar channel; a `-> Gc` row here is a box the caller would \
                 have to unwrap again, which is the whole thing this arm exists \
                 to remove",
                entry.receiver,
                entry.name,
                sym.name(),
            );
            assert!(
                matches!(entry.result, TypePattern::Scalar(_)),
                "{}.{} answers `{}`, which the scalar channel cannot carry",
                entry.receiver,
                entry.name,
                entry.result,
            );
            // A scalar primitive is not a safepoint in MIR, so a row that
            // allocates would be one the collector is never shown a frame at.
            assert!(
                !entry.allocates(),
                "{}.{} allocates, so it cannot be a non-safepoint instruction",
                entry.receiver,
                entry.name,
            );
        }
        assert_eq!(
            rows, 1,
            "`BitSet.contains` is the only scalar-primitive row today; a second \
             one wants an arm in `praxis-mir`'s `lower_scalar_primitive` before \
             this number moves"
        );
    }

    /// The two rows D1 answered, spelled out — so a future edit that quietly
    /// puts `Map.get` back to `V` fails *by name* as well as through the sweep.
    #[test]
    fn map_get_and_grid_find_answer_an_option() {
        let cat = builtin_catalog();
        let map_pat = TypePattern::Collection {
            ctor: CollectionCtor::Map,
            args: vec![TypePattern::var("K"), TypePattern::var("V")],
        };
        let get = cat
            .by_receiver_and_name(&map_pat, "get")
            .next()
            .expect("map.get exists");
        assert_eq!(
            get.result,
            TypePattern::Option(Box::new(TypePattern::var("V"))),
            "§5.7 writes `Map[K,V].get(K) -> Option[V]`"
        );

        let grid_pat = TypePattern::Collection {
            ctor: CollectionCtor::Grid,
            args: vec![TypePattern::var("T")],
        };
        let find = cat
            .by_receiver_and_name(&grid_pat, "find")
            .next()
            .expect("grid.find exists");
        assert_eq!(find.result, TypePattern::Option(Box::new(point_pattern())));

        // …and `Counter.get` keeps its zero default, which is not absence at
        // all: §6.2 says a counter's absent values *read as zero*.
        let counter_pat = counter_of_t();
        let counter_get = cat
            .by_receiver_and_name(&counter_pat, "get")
            .next()
            .expect("counter.get exists");
        assert_eq!(
            counter_get.result,
            TypePattern::Scalar(ScalarType::Int),
            "§6.2: a Counter's absent values read as zero, deliberately"
        );
    }

    /// **ADR-136, the catalog half.** `Text.int()` and `Text.float()` exist and
    /// each answers an `Option`.
    ///
    /// `Y001`'s help has promised `.int()` since it was written, and `Text`'s
    /// rows were `len`, `is_empty` and `get` — so half of the help for the most
    /// common mistake in a puzzle program sent the reader to a `Y110`. Pure
    /// data, so this goes red on the catalog edit alone.
    ///
    /// The two are asserted together because the pair is the decision: a
    /// language with a text-to-`Int` conversion and no text-to-`Float` one is a
    /// language where "read a number out of text" has a different answer per
    /// type, which is the asymmetry `parse(t, int)`/`parse(t, float)` does not
    /// have.
    #[test]
    fn text_int_and_float_answer_options() {
        let cat = builtin_catalog();
        for (name, scalar) in [("int", ScalarType::Int), ("float", ScalarType::Float)] {
            let entry = cat
                .by_receiver_and_name(&TypePattern::Scalar(ScalarType::Text), name)
                .next()
                .unwrap_or_else(|| panic!("`Text.{name}()` exists"));
            assert_eq!(
                entry.result,
                TypePattern::Option(Box::new(TypePattern::Scalar(scalar))),
                "a text that is not a number is absence, not a fault (\u{00a7}4.7)"
            );
            assert!(entry.params.is_empty(), "`{name}` takes no arguments");
        }
    }

    /// **ADR-086, the catalog half.** `t[i]` and `t.get(i)` answer a `Char`.
    ///
    /// This is pure data, so it goes red on the catalog edit alone and stays red
    /// whatever the runtime does — which is what makes it the *catalog's* gate.
    /// Its runtime twin is `text_get_answers_a_char_object` in
    /// `praxis-runtime`'s `abi.rs`, and neither can see the other's half.
    ///
    /// Observed red with the fix removed: with `result` back at
    /// `Scalar(Int)` both assertions fail, naming the row.
    #[test]
    fn the_two_text_reads_answer_a_char() {
        let cat = builtin_catalog();
        let text = text_receiver();

        for name in [crate::catalog::INDEX_READ, "get"] {
            let row = cat
                .by_receiver_and_name(&text, name)
                .next()
                .unwrap_or_else(|| panic!("Text.{name} exists"));
            assert_eq!(
                row.result,
                TypePattern::Scalar(ScalarType::Char),
                "ADR-086: `Text.{name}` answers a Char, not the char's scalar value"
            );
            // The two spellings are one answer, so they are one wrapper — unlike
            // `Map`, whose two reads are two wrappers on purpose (§4.7).
            assert_eq!(
                row.lowering,
                MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::TextGet),
                "`Text.{name}` lowers through the one text read"
            );
        }
    }

    /// **ADR-086, the conversion pair.** A one-way conversion would make
    /// `Grid[Char]`, `Vec[Char]` and `Map[Char, _]` write-only from the
    /// language's side, so the pair is asserted as a pair — the same shape
    /// §4.12 gives `Float.to_int`/`Int.to_float`.
    ///
    /// Observed red with the fix removed: neither row resolves, and both
    /// `expect`s fire.
    #[test]
    fn char_and_int_convert_both_ways() {
        let cat = builtin_catalog();

        let to_int = cat
            .by_receiver_and_name(&TypePattern::Scalar(ScalarType::Char), "to_int")
            .next()
            .expect("Char.to_int exists");
        assert_eq!(to_int.result, TypePattern::Scalar(ScalarType::Int));
        assert_eq!(to_int.purity, Purity::Pure);

        let to_char = cat
            .by_receiver_and_name(&TypePattern::Scalar(ScalarType::Int), "to_char")
            .next()
            .expect("Int.to_char exists");
        assert_eq!(to_char.result, TypePattern::Scalar(ScalarType::Char));

        // The narrowing direction is the one that can fail — `Int.to_char` is to
        // `Char.to_int` what `Float.to_int` is to `Int.to_float`. The manifest is
        // where that is enforced, so read it there rather than restating it.
        assert!(
            to_char.can_fault(),
            "not every Int is a Unicode scalar value, so the narrowing faults"
        );
        assert!(
            !to_int.can_fault(),
            "every Unicode scalar value fits an Int, so the widening cannot"
        );
    }

    /// **ADR-143.** The `to_text` family is `Int`, `Float` and `Char`, and it is
    /// closed at three.
    ///
    /// The closure is the half worth asserting: two thirds of this family were
    /// absent for two milestones on the argument that it "wants taking whole",
    /// and the next reader to arrive with a `Bool.to_text()` or a universal one
    /// should meet a failing test rather than an empty space. Adding either is a
    /// decision, not a completion.
    #[test]
    fn the_to_text_family_is_int_float_and_char() {
        let cat = builtin_catalog();
        for scalar in [ScalarType::Int, ScalarType::Float, ScalarType::Char] {
            let receiver = TypePattern::Scalar(scalar);
            let row = cat
                .by_receiver_and_name(&receiver, "to_text")
                .next()
                .unwrap_or_else(|| panic!("{scalar:?}.to_text exists"));
            assert_eq!(row.result, TypePattern::Scalar(ScalarType::Text));
            assert_eq!(row.purity, Purity::Pure);
            assert!(row.allocates(), "{scalar:?}.to_text answers a fresh Text");
            // Each renders a payload that was validated at construction, so
            // there is nothing left to fault on — and a faulting row would put a
            // `CheckFault` after every call site that can never fire.
            assert!(!row.can_fault(), "{scalar:?}.to_text cannot fail");
        }
        for scalar in [ScalarType::Bool, ScalarType::Byte, ScalarType::Text] {
            let receiver = TypePattern::Scalar(scalar);
            assert_eq!(
                cat.by_receiver_and_name(&receiver, "to_text").count(),
                0,
                "the `to_text` family is three scalars; {scalar:?} is not one of \
                 them, and a universal `to_text` is §8.1 interpolation's question"
            );
        }
    }

    /// **ADR-144.** `join` is one row on the generic receiver bounded to `Text`
    /// items; the sequence-of-`Char` case is `Vec[Char].to_text()` under a
    /// different name.
    ///
    /// The two negatives are the point. A concrete `Vec[Char].join/1` beside the
    /// generic row is refused by `finish` as `AmbiguousWithIterable`, and a
    /// second `Iterable` row differing only in its item bound would resolve by
    /// insertion order — so the surface had to be one `join` plus a different
    /// name, and this says the shape was chosen rather than settled for.
    #[test]
    fn join_is_one_row_and_a_sequence_of_chars_has_its_own_name() {
        let cat = builtin_catalog();

        let join: Vec<_> = cat.entries().iter().filter(|e| e.name == "join").collect();
        assert_eq!(join.len(), 1, "`join` is one row");
        assert_eq!(join[0].receiver, iterable_of_text_elem());
        assert_eq!(join[0].params, vec![TypePattern::Scalar(ScalarType::Text)]);
        assert_eq!(join[0].result, TypePattern::Scalar(ScalarType::Text));

        // No `to_text` row generalizes. It cannot: `Text` is one of the ten
        // pipeline receivers, so an `Iterable.to_text/0` would shadow every
        // scalar row of ADR-143's family at once.
        assert!(
            !cat.entries()
                .iter()
                .any(|e| e.name == "to_text" && matches!(e.receiver, TypePattern::Iterable { .. })),
            "an `Iterable.to_text` would collide with the scalar `to_text` rows"
        );
        let vec_of_char = TypePattern::Collection {
            ctor: CollectionCtor::Vec,
            args: vec![TypePattern::is_scalar("T", ScalarType::Char)],
        };
        let chars = cat
            .by_receiver_and_name(&vec_of_char, "to_text")
            .next()
            .expect("Vec[Char].to_text exists");
        assert_eq!(chars.result, TypePattern::Scalar(ScalarType::Text));
        assert!(chars.params.is_empty());
    }

    /// **ADR-145.** `reversed` carries no capability bound, and that is the
    /// row's claim rather than an oversight.
    ///
    /// `sorted` needs `Ord` because it calls `compare`, `unique` needs
    /// `HashStable` because it calls `hash` and `equals`. Reversal calls
    /// nothing, so a `Vec` of closures reverses — and a later edit that "tidies
    /// up" by giving it a bound to match its neighbours would take that away
    /// with no wrapper behaviour to justify it.
    #[test]
    fn reversed_is_a_barrier_with_no_bound_on_its_element() {
        let cat = builtin_catalog();
        let receiver = iterable_of_t();
        let row = cat
            .by_receiver_and_name(&receiver, "reversed")
            .next()
            .expect("Iterable.reversed exists");
        assert_eq!(row.result, vec_of_t());
        assert_eq!(row.purity, Purity::Pure);
        assert!(row.bounds().is_empty(), "reversal reads no callback");
        assert!(
            !row.can_fault(),
            "there is no element `reversed` can be handed that it cannot reverse"
        );
        assert!(
            matches!(
                row.lowering,
                MethodLowering::RuntimeSymbol(abi::RuntimeSymbol::VecReversed)
            ),
            "a barrier is a runtime call, not a fused stage: reversal cannot \
             answer its first element until it has seen the last"
        );
    }

    /// **ADR-149.** The two groupings are barriers that answer `Vec[Vec[T]]`,
    /// carry no capability bound, and fault.
    ///
    /// Three claims, each of which a later edit could undo for a plausible
    /// reason, so each is asserted rather than described:
    ///
    /// * **The nesting.** A row declaring `Vec[T]` would flatten the answer and
    ///   nothing in the row's own text would look wrong.
    /// * **The absent bound.** `sorted` and `unique` sit beside these with one
    ///   each, and a tidying edit that gave these one to match would take away
    ///   the `Vec` of closures that groups today — with no wrapper behaviour to
    ///   justify it, because a grouping calls no descriptor callback.
    /// * **The fault.** It is the one place these differ from `reversed`, and it
    ///   is what makes `chunks(0)` observable at all: MIR emits a `CheckFault`
    ///   after a call only when the wrapper declares one, so an `Allocates` row
    ///   here would set `InvalidSize` into a context nothing reads and hand the
    ///   program a Unit sentinel typed as a `Vec[Vec[T]]` (ADR-088).
    #[test]
    fn a_grouping_answers_a_nested_vec_with_no_bound_and_can_fault() {
        let cat = builtin_catalog();
        let receiver = iterable_of_t();
        for (name, symbol) in [
            ("chunks", abi::RuntimeSymbol::VecChunks),
            ("windows", abi::RuntimeSymbol::VecWindows),
        ] {
            let row = cat
                .by_receiver_and_name(&receiver, name)
                .next()
                .unwrap_or_else(|| panic!("Iterable.{name} exists"));
            assert_eq!(
                row.result,
                vec_of_vec_of_t(),
                "`{name}` groups without flattening"
            );
            assert_eq!(row.params, vec![TypePattern::Scalar(ScalarType::Int)]);
            assert_eq!(row.purity, Purity::Pure);
            assert!(
                row.bounds().is_empty(),
                "a grouping reads no descriptor callback ({name})"
            );
            assert!(
                row.can_fault(),
                "`{name}(0)` names no run, and the program has to be able to see that"
            );
            assert!(
                matches!(row.lowering, MethodLowering::RuntimeSymbol(s) if s == symbol),
                "a grouping is a barrier: it cannot answer its first group from \
                 one element ({name})"
            );
        }
    }

    /// **REP-46.** §4.12's overflow alternatives are three modes over three
    /// operators, and the table says exactly that — no more and no fewer.
    ///
    /// This is the enforcement of a rule §4.12 states and nothing else should
    /// restate. It asserts the closures as well as the members, because the
    /// closures are the half a reader is most likely to undo by adding the
    /// "obviously missing" `checked_div` — which would contradict §4.12's own
    /// next sentence, "Division by zero always faults".
    ///
    /// **Not a gate for the six new rows** — `a_missing_row_is_a_missing_row`
    /// would not be red for them either, since a catalog test can only assert
    /// what is there. The gates for the behaviour are in `jit.rs`, at the
    /// boundaries where the ordinary operator faults. This one is red on a
    /// *later* edit that widens or narrows the family, which is the thing worth
    /// pinning once nine rows exist and a tenth looks natural.
    #[test]
    fn the_overflow_alternative_family_is_three_modes_over_three_operators() {
        let cat = builtin_catalog();
        let int = TypePattern::Scalar(ScalarType::Int);

        for mode in ["wrapping", "saturating", "checked"] {
            for op in ["add", "sub", "mul"] {
                let name = format!("{mode}_{op}");
                let row = cat
                    .by_receiver_and_name(&int, &name)
                    .next()
                    .unwrap_or_else(|| panic!("§4.12's family includes `Int.{name}`"));
                assert_eq!(row.params, vec![TypePattern::Scalar(ScalarType::Int)]);
                // None of the nine may fault: that is what an *alternative* to a
                // faulting operator means, and ADR-088's verifier rule turns it
                // into "no `CheckFault` follows the call".
                assert!(!row.can_fault(), "`{name}` is an alternative to faulting");

                let want = if mode == "checked" {
                    TypePattern::Option(Box::new(TypePattern::Scalar(ScalarType::Int)))
                } else {
                    TypePattern::Scalar(ScalarType::Int)
                };
                assert_eq!(row.result, want, "`{name}`'s result");
            }
        }

        // The two closures §4.12 draws, asserted as absences.
        for absent in [
            "wrapping_div",
            "saturating_div",
            "checked_div",
            "wrapping_rem",
            "checked_rem",
            "wrapping_neg",
            "checked_neg",
            "saturating_abs",
        ] {
            assert!(
                cat.by_receiver_and_name(&int, absent).next().is_none(),
                "§4.12 closes the family before `{absent}`: division's escape \
                 hatch is closed by \"Division by zero always faults\", and \
                 `_neg`/`_abs` are spelled with `0.wrapping_sub(x)`"
            );
        }
    }
}
