# ADR-043: JIT and parser metadata belongs to a reclaimable generation, and reclaiming one needs proof the heap is gone

**Date:** 2026-07-29
**Status:** Accepted
**Milestone:** Repair (foundation F13, stage S8 — MIR-12, MIR-13, DBG-05, DBG-06, IP-12)
**Closes:** the §10.5 "JIT generation arena" gap, which every prior milestone
deferred with a comment
**Amends:** ADR-023's plan-index encoding (a plan is now named by a
`NonZeroU32`, not by a position that can also mean failure)

## Context

Everything the backend and the input-parser compiler produced for the runtime
to read by raw pointer — record schemas, tuple schemas, field-name strings,
function names, debug-local metadata, parser plans, plan literals — was minted
with `Box::leak`, and the expensive parts were memoized in process-global
`OnceLock<Mutex<HashMap<…>>>`s. Forty-two `Box::leak` sites workspace-wide.

That was one decision serving two purposes, and it failed at both.

**As a lifetime story it was a leak with no ceiling.** A `run` leaks once and
exits, which is why it never mattered. A *debugger session* recompiles: `p EXPR`
JITs a synthetic function per command and `reload` recompiles the whole program,
so a session's metadata grew for as long as the session ran (MIR-13, DBG-05).

**As a caching story it was unsound.** `record_schema_for`'s map was keyed on a
bare `RecordDefId(u32)`, which is a *per-`TypeDb` positional index*. The
debugger mints a fresh `TypeDb` per `p` and per `reload`, so `RecordDefId(211)`
in one program names a different struct than in the next — and the cache handed
the second program the first one's schema. A schema is the field-descriptor
table `equals`/`hash`/`format` dispatch through (ADR-042), so a `Text` field
inheriting an `Int` field's descriptor is an `i64` load from a `Box<str>`
header, not a mislabel (MIR-12, DBG-06).

**And the parser plan slab had a third failure on top.** Registration pushed
onto an unbounded `Vec` and narrowed `Vec::len()` with an unchecked `as u32`,
while `praxis_run_parser` narrowed the id it read back out of a boxed `Int` the
same way. Worse, index `0` was simultaneously the first plan any program
registers and the sentinel `lower_parse` emitted when parser analysis *failed*
— so a `parse(...)` that did not type-check ran whichever parser happened to be
registered first (IP-12).

The obstacle to fixing any of this was never the arena. It was that live `GcRef`
payloads hold raw pointers into the metadata: `RecordPayload.schema`,
`TuplePayload.schema`, and — for the schemas the parser interpreter builds at
runtime — `RecordField.name` pointing straight into plan storage. S6 made that
sharp by giving `Heap` a `Drop` that finalizes: reclamation ordered wrongly is a
use-after-free during teardown, not a quiet leak (hazard H15).

## Decision 1: one `Generation` owns the arena *and* the caches

`praxis_codegen_cranelift::generation::Generation` is a `bumpalo::Bump` plus the
record-schema, tuple-schema, string and debug-metadata caches. A `Jit` holds one
behind an `Rc`, declared *after* `module` so the code is torn down first.

Putting the caches inside the generation is what fixes MIR-12/DBG-06, because a
cache entry can no longer outlive the type database that justified it. The key
is `(GenerationId, RecordDefId)` even though the generation half is redundant
while a generation owns its own map — the redundancy states the invariant, so a
later change that shares one map between generations cannot silently
reintroduce the bug.

`GenerationId` is a `NonZeroU32` minted from a process `AtomicU32`.

**Rejected:** keeping the process-global caches and adding a `TypeDb` identity
to the key. It fixes the correctness half and none of the lifetime half, and it
leaves two owners of the same data.

## Decision 2: reclamation takes a `HeapDrained`, and dropping leaks

`Generation::retire(Rc<Generation>, HeapDrained)` is the only route that frees
the arena. `HeapDrained` is minted by exactly one function,
`Runtime::teardown(self)`, which consumes the runtime and drops the heap —
running every finalizer. So "reclaim a generation whose objects are still live"
does not type-check.

The arena is a `ManuallyDrop<Bump>` and `Generation::drop` does nothing.
A generation that is merely dropped therefore **leaks**, deliberately: at the
destructor nothing knows whether a live `RecordPayload` still names the arena,
and leaking is exactly the pre-S8 behaviour. Forgetting to retire costs memory;
it cannot cost soundness. The CLI retires on all three exit paths and
`DebugSession::teardown` retires all three arenas.

`HeapDrained` is `Clone`, because one teardown legitimately retires several
arenas (the debugger has a main generation, an evaluation generation, and the
plans). What it proves is honest and narrow: *a* runtime was torn down. A
process holding two runtimes could tear down one and retire an arena the other
refers into. It is a guard rail against the ordering mistake, not a theorem
about aliasing, and the CLI and debugger each own exactly one `Runtime`.

**Rejected:** a `Drop` impl that frees. It makes the common path right and the
one path that matters — a `Jit` dropped while its values are still in the heap,
which is what `p EXPR` does on every command — a use-after-free.

**Rejected:** deferring reclamation entirely and only fixing the cache key. That
leaves DBG-05 and MIR-13 open with no mechanism to close them later.

## Decision 3: interning, not just reclaiming, is what bounds a session

Reclaiming at teardown does nothing for a session that never ends. So every
`Generation` allocator deduplicates: `alloc_str` interns, both schema builders
are caches, and `debug_local_metas` is keyed on the metadata's content.

The debugger then shares **one** generation across every `p EXPR` — a fresh
`Jit` per command, via `Jit::in_generation`, but the same arena. Compiling the
same expression twice produces byte-identical metadata, so the second time
allocates nothing at all. The gate is
`repeated_evaluation_stops_growing_the_generation`: it runs the real `p` path
twenty times after priming and asserts the arena does not move.

Sharing is also what keeps it *correct*. The values a `p` leaves in the session
heap point at schemas from the module that built them; that module is thrown
away immediately, and only a shared, un-retired generation keeps those pointers
good.

## Decision 4: a `PlanId` is a `NonZeroU32`, and registration can refuse

A `CompiledPlan` owns a `bumpalo` arena holding everything the plan's `&'static`
fields address, so dropping it reclaims the plan. `register_plan` is bounded by
`MAX_PLANS` and refuses *before* pushing, returning `TooManyPlans` for the HIR
to report as an ordinary diagnostic; a `const` assertion proves the resulting id
fits a `u32`.

`PlanId` is a `NonZeroU32` so the old `plan_index: 0` failure sentinel has no
encoding. `lower_parse`'s failure arm lowers to an error expression now, because
it cannot lower to anything else. `praxis_run_parser` reconstructs the id with a
checked `try_from` plus `PlanId::from_raw`, and a value that names no plan is a
`ParseFailed` fault rather than an index.

Retirement goes through `praxis_runtime::retire_parser_plans(&HeapDrained)`, in
the runtime crate because that is where the proof lives — `praxis-input-parser`
cannot depend on `praxis-runtime`, since the interpreter points the other way.
It drops the interpreter's schema cache and the plans **together**: those
schemas borrow their field names out of plan storage, so retiring either alone
dangles the other. Making that possible is why the interpreter's schemas stopped
being `Box::leak`ed and started owning their fields.

## Consequences

- **A `&'static` on a schema field is now a lifetime erasure, not a fact.**
  `RecordSchema::fields`, `TupleSchema::descriptors` and the plan node types all
  still declare `&'static`, and the data lives in an arena. The erasure is
  contained: four functions in `Generation`, two in `plan.rs`, two in the
  runtime's parser schema cache. Each carries the same discharge — the arena
  outlives the pointers, and `retire`'s proof obligation is what says so. Making
  the fields raw slices instead would spread the unsafety across every reader
  rather than concentrating it at the writers; that is the trade taken, and it
  is worth revisiting if F12 reshapes these types anyway.

- **Records built in different generations no longer compare equal.**
  `record_equals` compares schema *pointers*, so a record `main` built and a
  record a `p EXPR` built are now distinct shapes where the global cache would
  have (sometimes correctly, sometimes not) shared one. That is RT-12's finding
  and S10's job: schema identity should be nominal or structural, not
  allocational. Until then the debugger's shared evaluation generation keeps
  every `p`-built record on one schema.

- **`tuples::POINT` stays a process-static leak.** It is one schema for every
  grid position, minted by the runtime rather than by a compile, and there is no
  generation to hang it on. Bounded at one, so it is not a growth problem.

- **`leak_static_str` is gone**, and `embed_text`'s comment promising "a
  `JitGeneration` arena (§10.5) … M-later" now names the thing that exists.

- **Un-ignored:** `record_schema_cache_is_scoped_by_type_database_not_bare_def_id`.
