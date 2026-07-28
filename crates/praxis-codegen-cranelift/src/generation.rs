//! One reclaimable JIT generation: the arena that owns every piece of metadata
//! generated code and the runtime read by raw pointer (F13, §10.5).
//!
//! Before S8, the backend minted its metadata with `Box::leak` and cached the
//! expensive parts in process-global `OnceLock<Mutex<HashMap<…>>>`s. That had
//! two consequences, one a leak and one a correctness bug:
//!
//! * **MIR-13/DBG-05** — a `reload` or a `p EXPR` compiles a whole new program,
//!   and everything the previous one leaked stayed leaked. Nothing was ever
//!   reclaimable, so a long debugger session grew without bound.
//! * **MIR-12/DBG-06** — the record-schema cache was keyed on a bare
//!   `RecordDefId(u32)`, which is a *per-`TypeDb` positional index*. The
//!   debugger mints a fresh `TypeDb` per `p` and per `reload`, so
//!   `RecordDefId(0)` in one session names a different struct than in the next
//!   and the cache handed back a schema built for the wrong shape — whose field
//!   descriptors then read a `Text` header as an `i64`.
//!
//! A [`Generation`] fixes both by being *the* owner: an arena plus the caches,
//! so a cache entry cannot outlive the type database that justified it, and the
//! whole thing can be handed back to the allocator at once.
//!
//! # Reclamation is proof-gated
//!
//! Live `GcRef` payloads hold raw pointers into this arena
//! ([`RecordPayload::schema`](praxis_runtime::RecordPayload),
//! [`TuplePayload::schema`](praxis_runtime::TuplePayload)), so reclamation must
//! be ordered strictly *after* heap teardown (hazard H15). That is encoded, not
//! documented: [`Generation::retire`] takes a
//! [`HeapDrained`](praxis_runtime::HeapDrained), which only
//! [`Runtime::teardown`](praxis_runtime::Runtime::teardown) can mint.
//!
//! A generation that is merely *dropped* leaks its arena — deliberately. That
//! is exactly the pre-S8 behaviour, so forgetting to retire costs memory rather
//! than soundness.
//!
//! # Everything is interned
//!
//! `alloc_str`, the schema builders and [`Generation::debug_local_metas`] all
//! deduplicate. Compiling the same program into the same generation twice
//! therefore allocates nothing the second time, which is what bounds a
//! debugger session: the REPL shares one generation across every `p EXPR`
//! (see `praxis_debugger::evaluate`), so a hundred repetitions of one
//! expression cost what one costs.

use std::cell::RefCell;
use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};

use bumpalo::Bump;
use praxis_runtime::descriptor::TypeDescriptor;
use praxis_runtime::records::{RecordField, RecordSchema};
use praxis_runtime::tuples::TupleSchema;
use praxis_runtime::{DebugLocalMeta, HeapDrained};

/// A process-unique identity for one JIT generation.
///
/// `NonZeroU32` so "no generation" is not spelled `0`, and so the id can be
/// half of a schema-cache key without a sentinel value in the space.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct GenerationId(NonZeroU32);

impl GenerationId {
    /// Mint the next id. Wraps only after 4 billion generations, at which point
    /// the process has bigger problems; `NonZeroU32::new` refuses `0` and we
    /// skip it.
    fn mint() -> GenerationId {
        static NEXT: AtomicU32 = AtomicU32::new(1);
        let mut raw = NEXT.fetch_add(1, Ordering::Relaxed);
        if raw == 0 {
            raw = NEXT.fetch_add(1, Ordering::Relaxed);
        }
        GenerationId(NonZeroU32::new(raw).expect("generation ids skip zero"))
    }

    /// The raw id, for diagnostics and for keying data structures elsewhere.
    pub fn get(self) -> u32 {
        self.0.get()
    }
}

/// The key a record schema is cached under: the generation *and* the def id.
///
/// The generation half is what MIR-12/DBG-06 were missing. It is redundant
/// while a generation owns its own map — and that redundancy is the point: the
/// key states the invariant, so a future change that shares one map between
/// generations cannot silently reintroduce the bug.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct RecordKey(GenerationId, u32);

/// Statistics about what a generation is holding, for tests and diagnostics.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GenerationStats {
    /// Bytes the arena has handed out (not counting chunk overhead).
    pub allocated_bytes: usize,
    /// Distinct interned strings.
    pub strings: usize,
    /// Distinct record schemas built.
    pub record_schemas: usize,
    /// Distinct tuple schemas built.
    pub tuple_schemas: usize,
    /// Distinct debug-local-metadata arrays built.
    pub debug_meta_arrays: usize,
}

/// One JIT generation's arena and metadata caches.
///
/// Held behind an [`Rc`] by every [`Jit`](crate::Jit) so a host can compile
/// several modules into one generation — which is how the debugger keeps
/// repeated `p EXPR` bounded.
pub struct Generation {
    id: GenerationId,
    /// `ManuallyDrop` so that dropping an un-retired generation *leaks* the
    /// arena rather than freeing storage a live `RecordPayload` still names.
    /// [`Generation::retire`] is the only thing that frees it.
    arena: ManuallyDrop<Bump>,
    strings: RefCell<HashMap<Box<str>, *const str>>,
    record_schemas: RefCell<HashMap<RecordKey, *const RecordSchema>>,
    tuple_schemas: RefCell<TupleSchemaCache>,
    debug_metas: RefCell<DebugMetaCache>,
}

/// Tuple schemas, keyed by their element-descriptor sequence (structural shape).
type TupleSchemaCache = HashMap<Box<[*const TypeDescriptor]>, *const TupleSchema>;

/// Debug-local metadata arrays, keyed by content, valued as `(ptr, len)`.
type DebugMetaCache = HashMap<Box<[DebugMetaKey]>, (*const DebugLocalMeta, usize)>;

/// The comparable projection of a [`DebugLocalMeta`], for interning.
///
/// Hashing the struct's bytes would read its `#[repr(C)]` padding; this spells
/// the fields out instead. `source_name` is compared as an address because
/// names are interned, so equal names are one pointer.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct DebugMetaKey {
    source_name: usize,
    name_len: u32,
    symbol_id: u32,
    descriptor: usize,
    type_id: u32,
    kind: u8,
    span_start: u32,
    span_end: u32,
}

impl DebugMetaKey {
    fn of(m: &DebugLocalMeta) -> DebugMetaKey {
        DebugMetaKey {
            source_name: m.source_name as usize,
            name_len: m.name_len,
            symbol_id: m.symbol_id,
            descriptor: m.descriptor as usize,
            type_id: m.type_id,
            kind: m.kind,
            span_start: m.span_start,
            span_end: m.span_end,
        }
    }
}

impl Generation {
    /// A fresh, empty generation with a process-unique id.
    pub fn new() -> Generation {
        Generation {
            id: GenerationId::mint(),
            arena: ManuallyDrop::new(Bump::new()),
            strings: RefCell::new(HashMap::new()),
            record_schemas: RefCell::new(HashMap::new()),
            tuple_schemas: RefCell::new(HashMap::new()),
            debug_metas: RefCell::new(HashMap::new()),
        }
    }

    /// This generation's identity.
    pub fn id(&self) -> GenerationId {
        self.id
    }

    /// What the generation is currently holding.
    pub fn stats(&self) -> GenerationStats {
        GenerationStats {
            allocated_bytes: self.arena.allocated_bytes(),
            strings: self.strings.borrow().len(),
            record_schemas: self.record_schemas.borrow().len(),
            tuple_schemas: self.tuple_schemas.borrow().len(),
            debug_meta_arrays: self.debug_metas.borrow().len(),
        }
    }

    /// Reclaim the arena. Requires proof that the heap holding every object
    /// that could name it has been dropped (hazard H15).
    ///
    /// A generation still shared with another [`Jit`](crate::Jit) is left
    /// alone — it will be retired when the last handle goes, or leak if none
    /// ever is.
    pub fn retire(this: Rc<Generation>, _proof: HeapDrained) {
        if let Ok(mut owned) = Rc::try_unwrap(this) {
            // SAFETY: `_proof` is a `HeapDrained`, minted only by
            // `Runtime::teardown`, which drops the heap and so finalizes every
            // payload that could hold a `*const RecordSchema` or
            // `*const TupleSchema` into this arena. Nothing dereferences the
            // arena after this point: the caches are dropped with `owned`, and
            // generated code that embedded these addresses lives in the
            // `JITModule`, which `Jit` declares *before* the generation and
            // therefore drops first.
            unsafe { ManuallyDrop::drop(&mut owned.arena) };
        }
    }

    /// Copy `s` into the arena, returning a shared, deduplicated reference.
    ///
    /// The returned lifetime is `'static` because the runtime structs that
    /// consume it (`RecordField::name`, and the `(ptr, len)` pair the debug
    /// frame carries) declare `&'static str`. It is a *generation* lifetime in
    /// truth; [`Generation::retire`]'s proof obligation is what makes the
    /// erasure sound. This is the one place it happens for strings.
    pub fn alloc_str(&self, s: &str) -> &'static str {
        if let Some(&interned) = self.strings.borrow().get(s) {
            // SAFETY: the target lives in this generation's arena; see the
            // lifetime note above.
            return unsafe { &*interned };
        }
        let copied: &str = self.arena.alloc_str(s);
        let raw: *const str = copied;
        self.strings.borrow_mut().insert(s.into(), raw);
        // SAFETY: as above.
        unsafe { &*raw }
    }

    /// The record schema for `def_id` in this generation, built by `fields` on
    /// first request and shared afterwards.
    ///
    /// `fields` is a closure so a cache hit does not pay for descriptor
    /// resolution — and so the caller can fail: a field whose type has no
    /// runtime object is a compile error (D9), not a schema with a wrong
    /// descriptor in it.
    pub fn record_schema<E>(
        &self,
        def_id: u32,
        fields: impl FnOnce() -> Result<Vec<RecordField>, E>,
    ) -> Result<*const RecordSchema, E> {
        let key = RecordKey(self.id, def_id);
        if let Some(&hit) = self.record_schemas.borrow().get(&key) {
            return Ok(hit);
        }
        let fields = fields()?;
        let stored: &[RecordField] = self.arena.alloc_slice_fill_iter(fields);
        // SAFETY (lifetime erasure): `RecordSchema::fields` declares `&'static`
        // and the slice lives in this generation's arena; see `alloc_str`.
        let stored: &'static [RecordField] = unsafe { &*(stored as *const [RecordField]) };
        let schema: &RecordSchema = self.arena.alloc(RecordSchema { fields: stored });
        let raw: *const RecordSchema = schema;
        self.record_schemas.borrow_mut().insert(key, raw);
        Ok(raw)
    }

    /// The tuple schema for an element-descriptor sequence, built once per
    /// distinct shape.
    ///
    /// Keyed structurally rather than by static `Type` id because the type
    /// arena does not intern tuples: two `(Int, Int)` literals get different
    /// `Type` ids and must still share a schema, or two equal tuples would
    /// compare unequal.
    pub fn tuple_schema(&self, descriptors: &[*const TypeDescriptor]) -> *const TupleSchema {
        if let Some(&hit) = self.tuple_schemas.borrow().get(descriptors) {
            return hit;
        }
        let stored: &[*const TypeDescriptor] = self.arena.alloc_slice_copy(descriptors);
        // SAFETY (lifetime erasure): `TupleSchema::descriptors` declares
        // `&'static` and the slice lives in this generation's arena.
        let stored: &'static [*const TypeDescriptor] =
            unsafe { &*(stored as *const [*const TypeDescriptor]) };
        let schema: &TupleSchema = self.arena.alloc(TupleSchema {
            descriptors: stored,
        });
        let raw: *const TupleSchema = schema;
        self.tuple_schemas
            .borrow_mut()
            .insert(descriptors.into(), raw);
        raw
    }

    /// Store one function's debug-local metadata array, deduplicated by
    /// content, and return `(ptr, len)` for the prologue's
    /// `praxis_push_debug_frame` call.
    ///
    /// Deduplication is what makes repeated compilation of the same source into
    /// one generation cost nothing (DBG-05): the same function lowered twice
    /// yields the same metadata, down to the interned name pointers.
    pub fn debug_local_metas(&self, metas: Vec<DebugLocalMeta>) -> (*const DebugLocalMeta, usize) {
        let key: Box<[DebugMetaKey]> = metas.iter().map(DebugMetaKey::of).collect();
        if let Some(&hit) = self.debug_metas.borrow().get(&key) {
            return hit;
        }
        let len = metas.len();
        let stored: &[DebugLocalMeta] = self.arena.alloc_slice_fill_iter(metas);
        let entry = (stored.as_ptr(), len);
        self.debug_metas.borrow_mut().insert(key, entry);
        entry
    }
}

impl Default for Generation {
    fn default() -> Self {
        Generation::new()
    }
}

impl std::fmt::Debug for Generation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Generation")
            .field("id", &self.id)
            .field("stats", &self.stats())
            .finish()
    }
}

impl Drop for Generation {
    /// Deliberately leaks the arena.
    ///
    /// A `Generation` reaching its destructor has *not* been handed a
    /// [`HeapDrained`], so nothing here knows whether a live `RecordPayload`
    /// still points into the arena. Leaking is the pre-S8 behaviour and is
    /// safe; freeing would be a use-after-free at the next `==`.
    /// [`Generation::retire`] is the route that actually reclaims.
    fn drop(&mut self) {
        // `arena` is `ManuallyDrop`: doing nothing here is the leak.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use praxis_runtime::Runtime;

    /// Two generations are two identities, and neither is zero — so a
    /// `(GenerationId, RecordDefId)` key cannot be forged from a bare def id.
    #[test]
    fn generation_ids_are_distinct_and_nonzero() {
        let a = Generation::new();
        let b = Generation::new();
        assert_ne!(a.id(), b.id());
        assert!(a.id().get() > 0 && b.id().get() > 0);
    }

    /// The same record def id in two generations gets two schemas. This is
    /// MIR-12/DBG-06 in miniature: `RecordDefId(0)` means different things in
    /// different type databases, so sharing a schema across them is the bug.
    #[test]
    fn the_same_def_id_in_two_generations_gets_two_schemas() {
        let a = Generation::new();
        let b = Generation::new();
        let build = || -> Result<Vec<RecordField>, ()> {
            Ok(vec![RecordField {
                name: "value",
                descriptor: &praxis_runtime::scalars::INT,
            }])
        };
        let from_a = a.record_schema(0, build).unwrap();
        let from_b = b.record_schema(0, build).unwrap();
        assert!(
            !std::ptr::eq(from_a, from_b),
            "a bare def id must not name one schema across type databases"
        );
    }

    /// Within one generation, the same def id is one schema — record equality
    /// compares schema pointers, so two values of one type must share.
    #[test]
    fn one_def_id_in_one_generation_is_one_schema() {
        let gen = Generation::new();
        let mut built = 0;
        let mut build = || {
            built += 1;
            Ok::<_, ()>(vec![RecordField {
                name: "value",
                descriptor: &praxis_runtime::scalars::INT,
            }])
        };
        let first = gen.record_schema(7, &mut build).unwrap();
        let second = gen.record_schema(7, &mut build).unwrap();
        assert!(std::ptr::eq(first, second));
        assert_eq!(built, 1, "a cache hit must not rebuild the schema");
    }

    /// A failed field resolution is not cached as anything: the next request
    /// tries again rather than inheriting a half-built schema.
    #[test]
    fn a_failed_schema_build_caches_nothing() {
        let gen = Generation::new();
        let failed: Result<*const RecordSchema, &str> =
            gen.record_schema(3, || Err("no runtime object for `Range`"));
        assert_eq!(failed.unwrap_err(), "no runtime object for `Range`");
        assert_eq!(gen.stats().record_schemas, 0);
    }

    /// Tuple schemas are keyed by shape, so two same-shaped tuples share one —
    /// and different shapes never do.
    #[test]
    fn tuple_schemas_are_shared_by_shape() {
        let gen = Generation::new();
        let int: *const TypeDescriptor = &praxis_runtime::scalars::INT;
        let text: *const TypeDescriptor = &praxis_runtime::text::TEXT;
        let a = gen.tuple_schema(&[int, int]);
        let b = gen.tuple_schema(&[int, int]);
        let c = gen.tuple_schema(&[int, text]);
        assert!(std::ptr::eq(a, b));
        assert!(!std::ptr::eq(a, c));
        assert_eq!(gen.stats().tuple_schemas, 2);
    }

    /// Interning is what bounds a long debugger session: the same metadata
    /// requested a hundred times costs what it costs once (DBG-05, MIR-13).
    #[test]
    fn repeated_identical_metadata_stops_growing_the_arena() {
        let gen = Generation::new();
        let meta = |name: &'static str| DebugLocalMeta {
            source_name: name.as_ptr(),
            name_len: name.len() as u32,
            symbol_id: 0,
            descriptor: &praxis_runtime::scalars::INT,
            type_id: 1,
            kind: praxis_runtime::LOCAL_KIND_USER,
            span_start: 0,
            span_end: 4,
        };
        // Two rounds to prime every cache, then measure across a hundred more.
        for _ in 0..2 {
            let name = gen.alloc_str("main");
            gen.debug_local_metas(vec![meta(name)]);
            gen.tuple_schema(&[&praxis_runtime::scalars::INT]);
            gen.record_schema(0, || {
                Ok::<_, ()>(vec![RecordField {
                    name: gen.alloc_str("value"),
                    descriptor: &praxis_runtime::scalars::INT,
                }])
            })
            .unwrap();
        }
        let primed = gen.stats();
        for _ in 0..100 {
            let name = gen.alloc_str("main");
            gen.debug_local_metas(vec![meta(name)]);
            gen.tuple_schema(&[&praxis_runtime::scalars::INT]);
            gen.record_schema(0, || {
                Ok::<_, ()>(vec![RecordField {
                    name: gen.alloc_str("value"),
                    descriptor: &praxis_runtime::scalars::INT,
                }])
            })
            .unwrap();
        }
        assert_eq!(
            gen.stats(),
            primed,
            "a hundred repetitions must allocate nothing new"
        );
    }

    /// A retired generation gives its storage back. The proof is what orders it
    /// after heap teardown; without one, `retire` does not compile.
    #[test]
    fn a_retired_generation_releases_its_arena() {
        let gen = Rc::new(Generation::new());
        gen.alloc_str("a string that costs real bytes");
        assert!(gen.stats().allocated_bytes > 0);
        let proof = Runtime::new().teardown();
        Generation::retire(gen, proof);
        // Nothing to assert on the freed arena — reading it would be the bug
        // this test exists to allow. That it compiles only with a `HeapDrained`
        // is the property.
    }

    /// A generation still shared with another handle is not reclaimed, so the
    /// other handle's pointers stay valid.
    #[test]
    fn a_shared_generation_survives_a_partial_retire() {
        let gen = Rc::new(Generation::new());
        let other = Rc::clone(&gen);
        let text = other.alloc_str("still referenced");
        let proof = Runtime::new().teardown();
        Generation::retire(gen, proof);
        assert_eq!(text, "still referenced");
        assert_eq!(Rc::strong_count(&other), 1);
    }
}
