# Milestone 3 report & Milestone 4 handover

**Project:** Praxis
**Date:** 2026-07-23
**Status:** Milestone 3 complete and green. All four acceptance criteria met.
Ready to begin Milestone 4.

> **For a fresh context:** read this document, then `praxis_technical_design.md`
> (the contract — §12 GC, §11 descriptors, §10.3 calling convention, §19
> Milestone 4, §13.5 MIR). The rest of this file assumes you have NOT seen the
> M3 code yet and tells you what exists, where, and what to do next.

---

## 1. What Praxis is (unchanged from M2)

Praxis is a small, statically typed, garbage-collected language for Advent of
Code-style puzzle solving. Procedural + expression-oriented. Every runtime value
is a GC object referenced through a uniform `GcRef`. First-class input parsing
(`read lines(int)`). JIT-compiled via Cranelift. Ships with an LSP + VS Code
extension. No ownership, no traits, no operator overloading, no exceptions.

The pipeline is:

```
source -> parse -> resolve -> infer -> typed IR -> lower -> Cranelift -> execute
```

`parse`/`resolve`/`infer` are real (M1/M2). **The runtime heap + collector are
now real (this milestone, M3).** Cranelift JIT + MIR are Milestone 4.

The full spec is **`praxis_technical_design.md`** at the repo root. Treat it as
the contract; deliberate deviations go in `docs/decisions/` (rule 20.1).

---

## 2. How to work in this repo (unchanged)

```sh
cargo install just          # one-time
just ci                     # the full quality gate (fmt-check + clippy + test)
                            # — EXACTLY what GitHub Actions runs
just fmt        # reformat (modifies files; NOT part of ci)
just fmt-check  # verify formatting
just clippy     # lint, -D warnings
just test       # cargo test --workspace
just build      # cargo build --workspace
```

**⚠️ Resource note for agents (still load-bearing):** never run two `cargo`
commands concurrently. Test scoped (`-p <crate> --lib <module>`) before
`just test`.

**Two workspace rules from `AGENTS.md` override everything:**
1. *"Make illegal states unrepresentable"* — prefer types that structurally
   forbid bad data over runtime checks or conventions.
2. *"Test every language feature extensively"* — every new behavior ships with
   unit + integration tests.

---

## 3. What Milestone 3 delivered

All §19 Milestone-3 deliverables. Acceptance criteria met:

| Criterion | Result |
|---|---|
| Every runtime value in interpreter/runtime tests is a `GcRef` | ✅ every `Runtime::alloc_*` returns `GcRef`; the only way to read a scalar is `GcRef::as_int()` etc. — no public surface returns a bare scalar as a "value" |
| Scalar allocation, tracing, formatting, equality, and collection work | ✅ descriptors for Unit/Bool/Int/Byte/Char/Text; `collect_reclaims_unrooted_allocation`, `collect_preserves_rooted_allocation`, format/equals/hash tested |
| GC stress tests preserve nested references | ✅ `collect_preserves_nested_references` (Vec of Int) and `collect_handles_vec_of_vec` (Vec of Vec of Int) — both format readable after collection |
| No source-language storage slot or public runtime wrapper uses an unboxed scalar ABI | ✅ uniform `GcRef` everywhere; `GcRef` is `#[repr(transparent)]` over `NonNull<GcHeader>` (pointer-sized, FFI-safe — guarded by a test) |
| `cargo test --workspace` passes | ✅ **228 tests** (was 201 at M2) |
| `just ci` clean | ✅ |

### 3.1 What changed at the crate level

Only `praxis-runtime` changed (M3 does not touch the front end; the `praxis
check` pipeline is unchanged). New dependency: `bumpalo`.

| Module | What landed in M3 |
|---|---|
| `praxis-runtime/src/gc.rs` | **Real `GcHeader`** (typed descriptor ptr + tri-color `Cell<u8>` mark + payload size) + `GcRef` accessors (`header`/`descriptor`/`payload`). `GcRef` gains `PartialEq`/`Eq`/`Hash` by pointer identity. |
| `praxis-runtime/src/descriptor.rs` | **Real `DynamicHasher` impl (`StructHasher`)** + `hash_value` helper. The `Tracer` trait is now implemented (by the collector's worklist in `heap.rs`). |
| `praxis-runtime/src/scalars.rs` (new) | **Scalar descriptors** as `const`s: `UNIT`/`BOOL`/`INT`/`BYTE`/`CHAR`, each with `trace`/`drop_value`/`format`/`equals`/`hash`. Scalar `trace` is a no-op (leaf case). |
| `praxis-runtime/src/text.rs` (new) | **`TEXT` descriptor** for owned `Box<str>` payloads (ADR-013: source-slice form is M6). Non-trivial `drop_value`. |
| `praxis-runtime/src/collections.rs` (new) | **`VEC` descriptor** for `Vec[T]`. Payload = `VecPayload { element_descriptor, items: Box<[GcRef]> }` (§11.2: element type recorded in the payload). `trace` forwards every element — **the nested-reference proof**. |
| `praxis-runtime/src/heap.rs` (new) | **The collector.** `Heap { arena: Bump, live: RefCell<Vec<NonNull<GcHeader>>> }`. `alloc`/`alloc_with`/`alloc_immortal`, precise tri-color `collect` (mark via descriptor `trace` + worklist; sweep via `drop_value`), `stats`, `reset`. |
| `praxis-runtime/src/roots.rs` (new) | **Explicit root frames (ADR-012).** `RootSet` trait + RAII `RootScope` (chains to a parent). This is the seam M4's shadow-stack frame will also implement. |
| `praxis-runtime/src/immortal.rs` (new) | **`Immortals`** — pre-allocated `Unit`/`true`/`false`, kept out of the live set, never reclaimed (§4.3). |
| `praxis-runtime/src/context.rs` | **`Runtime` owner** (heap + immortals) with typed alloc helpers (`alloc_int`/`alloc_bool`/…/`alloc_vec`) and `GcRef` typed accessors (`as_int`/`as_bool`/…/`as_vec`/`format`/`equals`). `RuntimeContext::from_runtime` produces the `#[repr(C)]` context for future generated code. |

The DAG stays clean: `praxis-runtime` → `praxis-source`, `bumpalo` (no new
internal edges). `praxis-mir`/`praxis-codegen-cranelift` already depend on
`praxis-runtime` (wired at M0) and will consume the heap in M4.

### 3.2 Decisions recorded in M3 (in `docs/decisions/`)

- **ADR-011:** collector is **Bumpalo arena + side `live` registry**, precise
  non-moving mark-and-sweep, no write barrier. Freed memory is not returned to
  the arena until `reset` (acceptable until benchmarked, rule 20.11).
- **ADR-012:** **explicit root frames** for M3 (`RootScope`); the shadow-stack
  layers on in M4 via the same `RootSet` trait.
- **ADR-013:** **six scalar descriptors + a `Vec[T]` descriptor** in M3;
  owned-only `Text` (source-slice is M6); other collection descriptors (Deque,
  Heap, Grid, Map, Set, Counter) are M5.

---

## 4. Key types a fresh context should know

In `praxis-runtime`:
- **`Runtime`** — the M3 entry point. `Runtime::new()` allocates the heap +
  immortals. `alloc_int/bool/byte/char/unit/text/vec`, `collect(&roots)`,
  `context()` (→ `RuntimeContext` for generated code).
- **`Heap`** — `alloc<T: Copy>(desc, val)`, `alloc_with(desc, size, align, init)`
  (for `Drop` payloads), `collect(&dyn RootSet)`, `stats()`. `#[repr(C)]`.
- **`GcRef`** — uniform value reference. `as_int/as_bool/as_byte/as_char/as_text/
  as_vec` (panic on descriptor mismatch), `format(&mut Write)`, `equals(&GcRef)`
  (structural, §5.5), `descriptor()`, `payload<T>()`.
- **`GcHeader`** — `{ descriptor, mark: Cell<u8>, size }`, `#[repr(C)]`. Payload
  follows immediately; reach via `header.payload::<T>()`.
- **`TypeDescriptor`** — the vtable (§11.4): `id/name/size/align/trace/drop_value
  /format/equals: Option/hash: Option`. Built-in `const`s in `scalars`/`text`/
  `collections`.
- **`RootScope`** — RAII root frame. `scope.root(gcref)`. Implements `RootSet`.
- **`Immortals`** — `unit()`/`true_()`/`false_()`/`bool_(b)`.

Mark colors (`WHITE`/`GREY`/`BLACK`) live in `gc.rs` (`pub(crate)`). GREY is
reserved for future concurrent collection; M3 colors straight to BLACK.

---

## 5. Open questions / known limitations to resolve later

1. **Freed memory is not reused until `Heap::reset`.** Sweep removes the liveness
   registration and runs `drop_value`, but the Bumpalo arena only reclaims on
   reset (ADR-011). Fine for AoC-scale short-lived runs; revisit if a benchmark
   shows churn (rule 20.11).
2. **`Text` is owned-only (`Box<str>`).** Source-slice `Text` (`owner: GcRef,
   start, len`, §12.6) lands in M6 with the input-parser. The nested-trace
   guarantee is currently proven by `Vec[T]` instead (ADR-013).
3. **Only `Vec[T]` among collections.** Deque/Heap/Grid/Map/Set/Counter
   descriptors + their method surfaces are M5. `Vec[T]` has alloc/trace/drop/
   format/equals but **no** `push`/`get` runtime wrappers yet.
4. **Root sets are over-conservative in M3** (a `RootScope` roots everything
   pushed into it; no liveness analysis). M4 tightens this via MIR liveness
   (§12.3), plugging generated frames into `RootSet`.
5. **`RuntimeContext` is constructed but not yet consumed by generated code.**
   `pending_fault`/`debug_top` stay null (M4/M10). The fault protocol (§10.4) is
   wired in shape only.
6. **The static type system (`praxis-types`) and runtime descriptors
   (`TypeDescriptor`) are still separate.** They meet in M4 when lowering emits
   descriptor references — do not merge them yet (handover M2 §6.4).
7. **`TypeId`s in descriptors are hand-assigned constants** (`Unit`=0 … `Vec`=6).
   When M4/M5 generate descriptors for user types, allocation becomes systematic.

---

## 6. Milestone 4 — what to build

**From §19:** *MIR and Cranelift JIT backend.*

### 6.1 Where things plug in

| Work | Crate | Notes |
|---|---|---|
| MIR (basic blocks, `GcRef` slots, calls, allocs, safepoints) | `praxis-mir` (skeleton, `FILLED_AT_MILESTONE = 4`) | §13.5. MIR need not be SSA initially; the Cranelift lowering makes SSA. |
| Cranelift lowering | `praxis-codegen-cranelift` (skeleton; **`cranelift` dep not yet added**) | Add the dep. Every function: `fn(RuntimeContext*, GcRef...) -> GcRef` (§10.3). Register `praxis_alloc`/`praxis_int_add`/... symbols (§11.1). |
| Lower HIR → MIR | new pass, likely in `praxis-mir` or `praxis-hir` | Bridges the M2 typed tree to MIR. This is where static `Type` meets runtime `TypeDescriptor` (limitation 6 above). |

### 6.2 What M3 gives M4

- A working **`Heap`** to allocate into: generated code calls a runtime
  `praxis_alloc`-style wrapper that delegates to `Heap::alloc`/`alloc_with`.
- The **`RuntimeContext`** (`#[repr(C)]`, stable field offsets) to hand to every
  generated function as the hidden first arg.
- The **`RootSet`** trait: a generated shadow-stack frame implements it so
  `Heap::collect` walks live registers/slots (§12.3). MIR liveness (§12.3)
  computes the minimal root set per safepoint.
- **Safepoints** (§12.4): every GC allocation is already a safepoint in M3
  (alloc may trigger collection); M4 inserts the pending-fault + GC checks at
  call boundaries and (optionally) loop backedges.

### 6.3 Things explicitly NOT in M4

- Collection method surface (`push`/`get`/…) → **M5**.
- Input-parser DSL → **M6**. Records/enums/closures → **M7**.
- Debugger → **M10**. LSP → **M11**.

---

## 7. Repo map for a fresh context

```
praxis_technical_design.md      # THE CONTRACT — read §12, §11, §10.3, §19-M4, §13.5
AGENTS.md                       # the two overriding workspace rules
justfile                        # quality gate (just ci)
docs/
  decisions/                    # ADR-001..013 — read 011/012/013 for M3 choices
  handovers/                    # this file + 00/01/02 milestone handovers
crates/
  praxis-runtime/               # START HERE for M4 — Heap, descriptors, Runtime, roots
  praxis-mir/ praxis-codegen-cranelift/   # skeletons — M4 fills these
  praxis-hir/ praxis-types/ praxis-ast/   # the M2 front end (lowering source)
  praxis-source/ praxis-syntax/ praxis-parser/ praxis-cli/  # unchanged
  praxis-{input-parser,debugger,lsp}/     # skeletons (later)
tests/
  parser/ ui/, typecheck/, run-pass/, run-fault/  # M4 will start filling run-pass/run-fault
```

---

## 8. Quick start for the fresh context

```sh
# 1. Confirm everything is green before touching anything:
just ci

# 2. Read the contract sections that govern M4:
#    praxis_technical_design.md §13.5 (MIR), §10 (JIT/ABI), §19 Milestone 4.

# 3. Read ADR-011/012/013 to understand the heap/root/descriptor shapes you'll
#    allocate into and the RootSet seam your shadow-stack frame must implement.

# 4. First vertical slice: lower a literal `Int` expression to MIR → Cranelift,
#    call Runtime::alloc_int, return the GcRef, assert its value.

# 5. Branch off main:
git switch -c milestone-4
```

Good luck. The runtime now allocates, traces, formats, compares, and collects
real objects end to end; the JIT has a heap to allocate into and a context to
receive.
