//! One reclaimable JIT generation: the arena that owns every piece of metadata
//! generated code and the runtime read by raw pointer (§10.5).
//!
//! A [`Generation`] is *the* owner of that metadata: an arena plus the caches
//! that key into it, so a cache entry cannot outlive the type database that
//! justified it, and the whole thing can be handed back to the allocator at
//! once. A `reload` or a `p EXPR` compiles a whole new program, so anything
//! leaked per compile would grow a debugger session without bound.
//!
//! **A `RecordDefId` is a per-`TypeDb` positional index, not an identity.** The
//! debugger mints a fresh `TypeDb` per `p` and per `reload`, so `RecordDefId(0)`
//! in one session names a different struct than in the next; a schema cache
//! shared across generations would hand back a schema built for the wrong shape,
//! whose field descriptors then read a `Text` header as an `i64`. Owning the
//! caches per generation is what rules that out.
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
//! A generation that is merely *dropped* leaks its arena — deliberately, so
//! forgetting to retire costs memory rather than soundness.
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
use praxis_runtime::enums::{EnumSchema, EnumVariantShape};
use praxis_runtime::records::{RecordField, RecordSchema, SchemaIdentity};
use praxis_runtime::tuples::TupleSchema;
use praxis_runtime::{DebugLocalMeta, DebugSlotKind, FunctionDebugMeta, HeapDrained};

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
/// The generation half is what a bare def id lacks. It is redundant while a
/// generation owns its own map — and that redundancy is the point: the key
/// states the invariant, so a future change that shares one map between
/// generations cannot silently reintroduce a cross-`TypeDb` schema.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct RecordKey(GenerationId, u32);

/// The key an enum schema is cached under: the generation, the def id, **and**
/// the resolved payload-descriptor sequence of every variant.
///
/// The last part is what a record key does not need. `Option` is a *generic*
/// def, so one `EnumDefId` covers `Option[Int]` and `Option[Text]`, whose `Some`
/// slots must not share a schema — a schema is what `equals`/`hash` dispatch
/// through, and the wrong one there reads a `Text` header as an `i64`. The
/// generation half is [`RecordKey`]'s, for the same reason.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct EnumKey(GenerationId, u32, Box<[Box<[usize]>]>);

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
    /// Distinct enum schemas built.
    pub enum_schemas: usize,
    /// Distinct debug-local-metadata arrays built.
    pub debug_meta_arrays: usize,
    /// Distinct per-function debug metadata records built.
    pub function_debug_metas: usize,
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
    enum_schemas: RefCell<HashMap<EnumKey, *const EnumSchema>>,
    debug_metas: RefCell<DebugMetaCache>,
    function_metas: RefCell<HashMap<FunctionMetaKey, *const FunctionDebugMeta>>,
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
    /// The direct call this local is the result of, compared as an address for
    /// [`source_name`](Self::source_name)'s reason — callee names are interned
    /// too, so equal names are one pointer.
    ///
    /// Load-bearing for correctness, like `slot_kind` below and not like the
    /// span above it. Two `__fnvalue_*` adapters are identical in every other
    /// field — an empty name, a positional symbol id, no span at all — and
    /// differ only in the function they forward to, so interning them together
    /// would put one adapter's callee in the other's frame and point the
    /// debugger at a call that frame is not in.
    callee_name: usize,
    callee_name_len: u32,
    /// ADR-120 part 2. Load-bearing for *correctness*, not just for cache hit
    /// rate: two locals identical in every other field but disagreeing about
    /// whether their slot holds a reference are not one local, and interning
    /// them together would hand one function's frame the other's answer to the
    /// question the collector's post-sweep scan asks.
    slot_kind: DebugSlotKind,
}

/// The comparable projection of a [`FunctionDebugMeta`], for interning.
///
/// Same reasoning as [`DebugMetaKey`], one level up: spelled out rather than
/// hashed as bytes, and the two pointers compare as addresses because both are
/// already interned in this arena — the name by [`Generation::alloc_str`], the
/// locals array by [`Generation::debug_local_metas`]. So equal content really
/// is one address, and this cache is the last hop rather than a re-comparison.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct FunctionMetaKey {
    func_name: usize,
    func_name_len: u32,
    local_count: u32,
    locals: usize,
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
            callee_name: m.callee_name as usize,
            callee_name_len: m.callee_name_len,
            slot_kind: m.slot_kind,
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
            enum_schemas: RefCell::new(HashMap::new()),
            debug_metas: RefCell::new(HashMap::new()),
            function_metas: RefCell::new(HashMap::new()),
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
            enum_schemas: self.enum_schemas.borrow().len(),
            debug_meta_arrays: self.debug_metas.borrow().len(),
            function_debug_metas: self.function_metas.borrow().len(),
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
            // payload that could hold a `*const RecordSchema`, a
            // `*const TupleSchema` or a `*const EnumSchema` into this arena.
            // Nothing dereferences the arena after this point: the caches are
            // dropped with `owned`, and generated code that embedded these
            // addresses lives in the `JITModule`, which `Jit` declares *before*
            // the generation and therefore drops first.
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
        identity: SchemaIdentity,
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
        let schema: &RecordSchema = self.arena.alloc(RecordSchema {
            identity,
            fields: stored,
        });
        let raw: *const RecordSchema = schema;
        self.record_schemas.borrow_mut().insert(key, raw);
        Ok(raw)
    }

    /// The enum schema for `def_id` at one instantiation, built once per
    /// (def, payload-descriptor sequence).
    ///
    /// `variants` is the variant list in declaration order — the tag indexes it
    /// — each entry a name and its payload descriptors. Unlike
    /// [`record_schema`](Self::record_schema) the caller resolves before the
    /// lookup rather than behind a closure, because the resolved descriptors
    /// *are* the cache key: `Option[Int]` and `Option[Text]` are one def and
    /// must not be one schema.
    pub fn enum_schema(
        &self,
        def_id: u32,
        identity: SchemaIdentity,
        variants: Vec<(&'static str, Vec<*const TypeDescriptor>)>,
    ) -> *const EnumSchema {
        let key = EnumKey(
            self.id,
            def_id,
            variants
                .iter()
                .map(|(_, payload)| payload.iter().map(|d| *d as usize).collect())
                .collect(),
        );
        if let Some(&hit) = self.enum_schemas.borrow().get(&key) {
            return hit;
        }
        let shapes: Vec<EnumVariantShape> = variants
            .into_iter()
            .map(|(name, payload)| {
                let stored: &[*const TypeDescriptor] = self.arena.alloc_slice_copy(&payload);
                // SAFETY (lifetime erasure): `EnumVariantShape::payload` declares
                // `&'static` and the slice lives in this generation's arena; see
                // `alloc_str`.
                let stored: &'static [*const TypeDescriptor] =
                    unsafe { &*(stored as *const [*const TypeDescriptor]) };
                EnumVariantShape {
                    name,
                    payload: stored,
                }
            })
            .collect();
        let stored: &[EnumVariantShape] = self.arena.alloc_slice_fill_iter(shapes);
        // SAFETY (lifetime erasure): as above.
        let stored: &'static [EnumVariantShape] =
            unsafe { &*(stored as *const [EnumVariantShape]) };
        let schema: &EnumSchema = self.arena.alloc(EnumSchema {
            identity,
            variants: stored,
        });
        let raw: *const EnumSchema = schema;
        self.enum_schemas.borrow_mut().insert(key, raw);
        raw
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
    /// content, and return `(ptr, len)`.
    ///
    /// Deduplication is what makes repeated compilation of the same source into
    /// one generation cost nothing: the same function lowered twice yields the
    /// same metadata, down to the interned name pointers.
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

    /// Store one function's whole debug metadata record — name, source span and
    /// locals — deduplicated by content, and return the address the prologue
    /// writes into its [`DebugFrameEntry`](praxis_runtime::DebugFrameEntry).
    ///
    /// This is where ADR-104's static half lands. All of it is the same for
    /// every call of the function, so interning it lets the prologue name it
    /// with one immediate instead of passing six arguments per call, and a
    /// debugger session that recompiles the same function on every `p EXPR`
    /// still allocates nothing new — the property
    /// `repeated_identical_metadata_stops_growing_the_arena` pins.
    pub fn function_debug_meta(
        &self,
        func_name: &'static str,
        span: (u32, u32),
        metas: Vec<DebugLocalMeta>,
    ) -> *const FunctionDebugMeta {
        let (locals, local_count) = self.debug_local_metas(metas);
        let key = FunctionMetaKey {
            func_name: func_name.as_ptr() as usize,
            func_name_len: func_name.len() as u32,
            local_count: local_count as u32,
            locals: locals as usize,
            span_start: span.0,
            span_end: span.1,
        };
        if let Some(&hit) = self.function_metas.borrow().get(&key) {
            return hit;
        }
        let stored: &FunctionDebugMeta = self.arena.alloc(FunctionDebugMeta {
            func_name: func_name.as_ptr(),
            func_name_len: key.func_name_len,
            local_count: key.local_count,
            locals,
            span_start: span.0,
            span_end: span.1,
        });
        let raw: *const FunctionDebugMeta = stored;
        self.function_metas.borrow_mut().insert(key, raw);
        raw
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
    /// still points into the arena. Leaking is safe; freeing would be a
    /// use-after-free at the next `==`. [`Generation::retire`] is the route that
    /// actually reclaims.
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

    /// The same record def id in two generations gets two schemas:
    /// `RecordDefId(0)` means different things in different type databases, so
    /// sharing a schema across them would be the bug.
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
        let from_a = a
            .record_schema(0, SchemaIdentity::Nominal("P"), build)
            .unwrap();
        let from_b = b
            .record_schema(0, SchemaIdentity::Nominal("P"), build)
            .unwrap();
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
        let first = gen
            .record_schema(7, SchemaIdentity::Anonymous, &mut build)
            .unwrap();
        let second = gen
            .record_schema(7, SchemaIdentity::Anonymous, &mut build)
            .unwrap();
        assert!(std::ptr::eq(first, second));
        assert_eq!(built, 1, "a cache hit must not rebuild the schema");
    }

    /// A failed field resolution is not cached as anything: the next request
    /// tries again rather than inheriting a half-built schema.
    #[test]
    fn a_failed_schema_build_caches_nothing() {
        let gen = Generation::new();
        let failed: Result<*const RecordSchema, &str> =
            gen.record_schema(3, SchemaIdentity::Anonymous, || {
                Err("no runtime object for `Range`")
            });
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
    /// requested a hundred times costs what it costs once.
    #[test]
    fn repeated_identical_metadata_stops_growing_the_arena() {
        let gen = Generation::new();
        let meta = |name: &'static str| DebugLocalMeta {
            callee_name: std::ptr::null(),
            callee_name_len: 0,
            source_name: name.as_ptr(),
            name_len: name.len() as u32,
            symbol_id: 0,
            descriptor: &praxis_runtime::scalars::INT,
            type_id: 1,
            kind: praxis_runtime::LOCAL_KIND_USER,
            span_start: 0,
            span_end: 4,
            slot_kind: DebugSlotKind::Reference,
        };
        // Two rounds to prime every cache, then measure across a hundred more.
        for _ in 0..2 {
            let name = gen.alloc_str("main");
            gen.function_debug_meta(name, (0, 40), vec![meta(name)]);
            gen.tuple_schema(&[&praxis_runtime::scalars::INT]);
            gen.record_schema(0, SchemaIdentity::Anonymous, || {
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
            gen.function_debug_meta(name, (0, 40), vec![meta(name)]);
            gen.tuple_schema(&[&praxis_runtime::scalars::INT]);
            gen.record_schema(0, SchemaIdentity::Anonymous, || {
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

    /// Two locals that differ *only* in the call they are the result of are two
    /// locals, and interning them together would put one function's callee in
    /// the other's frame.
    ///
    /// This is not a hypothetical shape. A `__fnvalue_*` adapter's return slot
    /// has an empty name, a positional symbol id and no span at all, so two
    /// adapters over functions of the same type are identical in every field
    /// but this one — and the debugger would then place a caller's line on a
    /// call that frame is not in.
    #[test]
    fn two_locals_that_differ_only_in_their_callee_are_not_one_local() {
        let gen = Generation::new();
        let meta = |callee: &'static str| DebugLocalMeta {
            callee_name: callee.as_ptr(),
            callee_name_len: callee.len() as u32,
            source_name: "".as_ptr(),
            name_len: 0,
            symbol_id: 1,
            descriptor: &praxis_runtime::scalars::INT,
            type_id: 1,
            kind: praxis_runtime::LOCAL_KIND_TEMP,
            span_start: 0,
            span_end: 0,
            slot_kind: DebugSlotKind::Reference,
        };
        let double = gen.alloc_str("double");
        let triple = gen.alloc_str("triple");
        let (a, _) = gen.debug_local_metas(vec![meta(double)]);
        let (b, _) = gen.debug_local_metas(vec![meta(triple)]);
        let (a_again, _) = gen.debug_local_metas(vec![meta(double)]);
        assert!(!std::ptr::eq(a, b), "different callees, different metadata");
        assert!(std::ptr::eq(a, a_again), "the same callee still interns");
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
