# Milestone 4 report & Milestone 5 handover

**Project:** Praxis
**Date:** 2026-07-23
**Status:** Milestone 4 complete and green. All four acceptance criteria met.
Ready to begin Milestone 5.

> **For a fresh context:** read this document, then `praxis_technical_design.md`
> (the contract — §10 JIT/ABI, §11 descriptors, §12 GC, §13.5 MIR, §19
> Milestone 5). The rest of this file assumes you have NOT seen the M4 code yet
> and tells you what exists, where, and what to do next.

---

## 1. What Praxis is (unchanged from M3)

Praxis is a small, statically typed, garbage-collected language for Advent of
Code-style puzzle solving. Procedural + expression-oriented. Every runtime value
is a GC object referenced through a uniform `GcRef`. First-class input parsing
(`read lines(int)`). JIT-compiled via Cranelift. Ships with an LSP + VS Code
extension. No ownership, no traits, no operator overloading, no exceptions.

The pipeline is now **fully real end to end**:

```
source -> parse -> resolve -> infer -> typed HIR -> MIR -> Cranelift -> execute
```

`parse`/`resolve`/`infer` (M1/M2), the runtime heap + collector (M3), and now
the **typed HIR + MIR + Cranelift JIT + fault protocol** (this milestone, M4)
are all real. `praxis run <file>` JIT-compiles and executes.

The full spec is **`praxis_technical_design.md`** at the repo root. Treat it as
the contract; deliberate deviations go in `docs/decisions/` (rule 20.1).

---

## 2. How to work in this repo (unchanged)

```sh
cargo install just          # one-time
just ci                     # the full quality gate (fmt-check + clippy + test)
just fmt        # reformat (modifies files; NOT part of ci)
just test       # cargo test --workspace
```

**⚠️ Resource note for agents:** never run two `cargo` commands concurrently.
Test scoped (`-p <crate> --lib <module>`) before `just test`.

**Two workspace rules from `AGENTS.md` override everything:**
1. *"Make illegal states unrepresentable."*
2. *"Test every language feature extensively."*

---

## 3. What Milestone 4 delivered

All §19 Milestone-4 deliverables. Acceptance criteria met:

| Criterion | Result |
|---|---|
| MIR for object-based control flow | ✅ `praxis-mir`: basic blocks, `Gc`/`Scalar` slots, branches, loops, calls, allocs, safepoints, fault edges (ADR-015) |
| Cranelift lowering + `fn(RuntimeContext*, GcRef...) -> GcRef` | ✅ `praxis-codegen-cranelift` lowers MIR → Cranelift IR; every function follows the uniform ABI (§10.3) |
| JIT symbol registration + generated entry point | ✅ `JITBuilder::symbol` registers every `praxis_*`; `praxis run` invokes `main` |
| Boxed arithmetic, comparison, branching, loops, function calls | ✅ recursive `fact`/`fib`, `while` sums, `if/else`, checked `+ - * / %` |
| Pending-fault state + generated checks + source maps | ✅ `Fault { pending, kind }`; `praxis_check_fault`; overflow/div-by-zero set the fault and return a sentinel (§10.4) — **no Rust unwinding across the ABI** |
| **Execute boxed integer arithmetic, branches, loops, and recursive calls** | ✅ `praxis run` prints correct results; 10 JIT integration tests + 9 CLI corpus tests |
| **No object files or linker invocation** | ✅ Cranelift JIT; `JITBuilder::symbol` resolves imports in-process |
| **Overflow/div-by-zero return to host without Rust unwinding** | ✅ exit 1 with a fault message; a dedicated test asserts the process is *not* killed by a signal |
| **Named locals available as `GcRef` values in fault snapshots** | ✅ MIR `Local`s carry `debug_names`; liveness roots live `Gc` locals per safepoint (ADR-016) |
| `cargo test --workspace` passes | ✅ **270 tests** (was 228 at M3) |
| `just ci` clean | ✅ |

### 3.1 What changed at the crate level

Four crates changed; one pipeline stage became real.

| Crate / module | What landed in M4 |
|---|---|
| `praxis-hir/src/lower.rs` (new) | **Typed-HIR tree** (ADR-014). Lowers `Analysis` + AST into `TypedModule`/`TypedFn`/`TypedStmt`/`TypedExpr`, each node carrying its `Type`. Rejects generic fns with `Y100`. `Analysis` gains a public `decls` field (declaration-site → `SymbolId`). |
| `praxis-mir` | **The MIR** (ADR-015). `ir.rs` (Function/Block/Inst/Terminator, `Gc`/`Scalar` locals), `liveness.rs` (§12.3 per-safepoint minimal root set), `build.rs` (typed-HIR → MIR). Non-SSA; Cranelift makes SSA. |
| `praxis-runtime/src/abi.rs` | **The `praxis_*` ABI wrappers** (ADR-017). `#[no_mangle] extern "C"`: alloc/load/arith/cmp/check_fault. `Fault` becomes real (`{ pending, kind: FaultKind }`); checked arithmetic never panics. |
| `praxis-codegen-cranelift` | **The JIT** (ADR-015/016/017). `module.rs` (`Jit`: `JITBuilder` + symbol registration + compile/finalize/entry), `lower.rs` (MIR → Cranelift IR per function), `symbols.rs` (`praxis_*` name→ptr). |
| `praxis-cli/src/run.rs` (new) | **`praxis run`**: parse → analyze → typed HIR → MIR → JIT → execute `main` → print. Honesty gate: never JIT input with diagnostics. |

The DAG stays clean. New external dep: `cranelift` (umbrella) + frontend/jit/
module sub-crates at `0.134`, plus `thiserror`. `praxis-codegen-cranelift` →
`praxis-mir` → `praxis-hir` → (`praxis-ast`, `praxis-types`, `praxis-runtime`).

### 3.2 Decisions recorded in M4 (in `docs/decisions/`)

- **ADR-014:** a **typed-HIR tree** as the MIR lowering boundary; `Analysis.decls`
  exposed; `lower` takes `&mut Analysis` (instantiating schemes allocates slots).
- **ADR-015:** MIR is **non-SSA slots** (`Gc` vs transient `Scalar`); Cranelift's
  `Variable` + `seal_all_blocks` makes SSA. `ConstInt` folds literals.
- **ADR-016:** **MIR liveness** computes the minimal per-safepoint `Gc` root set,
  stored in each instruction's `live_roots`.
- **ADR-017:** **`praxis_*` ABI wrappers** + the no-panic fault protocol; `Fault`
  is real and owned by `Runtime`; `JITBuilder::symbol` resolves imports.
- **ADR-018:** **monomorphization deferred**; M4 is monomorphic-only, generics
  rejected with `Y100`.

---

## 4. Key types a fresh context should know

In `praxis-mir`:
- **`Function`** — `{ name, params, return_local, locals, blocks, debug_names }`.
  `new_local(kind, ty, debug_name)`, `new_block()`.
- **`Local` / `LocalKind`** — `Gc` (a `GcRef` slot, GC-rooted at safepoints) or
  `Scalar(ScalarKind)` (transient `i64`/`u8`/`u32`/`bool`; must not cross a safepoint).
- **`Inst`** — `ConstInt`, `Alloc`, `ExtractScalar`, `Materialize`, `IntBinOp`,
  `IntCmp`, `Call`, `CheckFault`, `MoveGc`, `StoreScalar`. Safepoints (`Alloc`/
  `Materialize`/`Call`) carry `live_roots: Vec<LocalId>`.
- **`Terminator`** — `Branch`/`Jump`/`Return`/`Fault`.
- **`lower_module(&TypedModule, &mut TypeDb) -> Vec<Function>`**, **`annotate(&mut Function)`**.

In `praxis-hir`:
- **`lower(file, &SourceFile, &mut Analysis) -> TypedModule`** — the typed tree.
- **`TypedExpr`** — `Lit`/`Path`/`Bin`/`Unary`/`Paren`/`Block`/`If`/`While`/`Call`/
  `Tuple`, each carrying `ty: Type`. `Call` carries `callee_name: String`.

In `praxis-runtime`:
- **`Fault` / `FaultKind`** — `{ pending: bool, kind: FaultKind }`; `FaultKind` =
  `None`/`IntOverflow`/`DivByZero`. `Runtime::fault()`/`has_pending_fault()`/`take_fault()`.
- **The `praxis_*` wrappers** in `abi.rs` — every `extern "C"` fn the JIT calls.

In `praxis-codegen-cranelift`:
- **`Jit::new()`** — constructs the `JITModule` with `praxis_*` symbols registered.
- **`Jit::compile(&[Function]) -> HashMap<String, FuncId>`** — declares, lowers,
  defines, finalizes. **`Jit::entry(FuncId) -> *const u8`** — the entry pointer.

---

## 5. Open questions / known limitations to resolve later

1. **GC rooting via the generated shadow-stack frame is specified but not yet
   spilled.** ADR-016's liveness populates `live_roots`, but the Cranelift
   backend does not yet emit the stack-slot spill of those roots into a
   `RootSet`-implementing frame. Collection is triggered only by the host's
   explicit `Runtime::collect`; the non-moving heap keeps referenced objects
   alive regardless, so M4 acceptance passes. M5 (or a follow-up) emits the spill.
2. **Arithmetic re-allocates around each faultable op.** `Extract → IntBinOp →
   re-Alloc/Materialize` is correct but allocates more than necessary. A peephole
   can elide the re-allocation between an extract and the next materialize.
3. **Logical `||`/`!` are non-short-circuiting placeholders** (rare in the M4
   corpus). Real short-circuit lowering needs branch lowering for the operands.
4. **Tuples/Text/Vec materialize but have no method surface.** M4 *allocates*
   them; `push`/`get`/… and the full collection API are **M5**.
5. **Monomorphization (§13.6) is deferred** (ADR-018). Generic functions are
   rejected with `Y100`. A real instantiation pass is a future milestone.
6. **Unary neg routes through `IntBinOp::Sub`** (`0 - x`); the `praxis_int_neg`
   wrapper exists but is not yet emitted directly. The MIR `IntNeg` path is a
   trivial follow-up.
7. **`out(...)` / stdout writing is not wired** — `main`'s returned value is
   printed by the host. The `praxis_write_stdout` symbol is M5+.
8. **Text literals leak for the JIT's lifetime** (`Box::leak`). A `JitGeneration`
   arena (§10.5) reclaims these in watch/debugger mode (later).

---

## 6. Milestone 5 — what to build

**From §19:** *Collection method surface and the remaining collection types.*

### 6.1 Where things plug in

| Work | Crate | Notes |
|---|---|---|
| Collection descriptors (Deque, Heap, Grid, Map, Set, Counter) | `praxis-runtime` (new descriptor modules) | §11.2/§11.3. `Vec[T]` already exists (M3); add the rest. |
| Collection method wrappers (`praxis_vec_push/get/…`, `praxis_map_get/insert`, …) | `praxis-runtime/src/abi.rs` | §11.1. The `.method()` dispatch catalog exists in `praxis-hir/catalog` (ADR-010 wired the bridge in M2). |
| `.method()` lowering in MIR + Cranelift | `praxis-mir`, `praxis-codegen-cranelift` | Method calls become `Inst::Call` to the matching `praxis_*` wrapper. |
| GC shadow-stack spill | `praxis-codegen-cranelift` | Emit the §12.3 frame from `live_roots` (limitation 1 above) — becomes load-bearing once collections allocate heavily. |

### 6.2 What M4 gives M5

- A working **JIT** that can call any new `praxis_*` wrapper just by registering
  its symbol and emitting a `Call` — the path is proven end to end.
- The **`praxis_*` ABI layer** as the single place to add `praxis_vec_push`,
  `praxis_map_get`, etc. — no new calling-convention work.
- The **typed-HIR tree** carrying types, so method-receiver type resolution is
  straightforward.
- The **liveness/root-set** infrastructure for the shadow-stack spill.

### 6.3 Things explicitly NOT in M5

- Input-parser DSL → **M6**. Records/enums/closures → **M7**.
- Debugger → **M10**. LSP → **M11**.

---

## 7. Repo map for a fresh context

```
praxis_technical_design.md      # THE CONTRACT — read §10, §11, §12, §13.5, §19-M5
AGENTS.md                       # the two overriding workspace rules
justfile                        # quality gate (just ci)
docs/
  decisions/                    # ADR-001..018 — read 014-018 for M4 choices
  handovers/                    # this file + 00/01/02/03 milestone handovers
crates/
  praxis-codegen-cranelift/     # the JIT — module.rs, lower.rs, symbols.rs
  praxis-mir/                   # the MIR — ir.rs, liveness.rs, build.rs
  praxis-hir/src/lower.rs       # the typed-HIR tree (MIR lowering source)
  praxis-runtime/src/abi.rs     # the praxis_* wrappers + fault protocol
  praxis-cli/src/run.rs         # `praxis run`
  praxis-runtime/               # Heap, descriptors, Runtime, roots (M3)
  praxis-hir/ praxis-types/ praxis-ast/   # the M2 front end
tests/
  run-pass/ run-fault/          # reserved (M4 corpus lives in praxis-cli/tests/fixtures/run/)
```

---

## 8. Quick start for the fresh context

```sh
# 1. Confirm everything is green before touching anything:
just ci

# 2. Run a program through the JIT:
./target/debug/praxis run path/to/program.px

# 3. Read the contract sections that govern M5:
#    praxis_technical_design.md §11 (descriptors + collection wrappers), §19 M5.

# 4. Read ADR-014..018 to understand the typed-HIR/MIR/JIT/fault shapes, and
#    ADR-010 for the method-catalog bridge already wired in M2.

# 5. First vertical slice: add a Vec[T] `push`/`len` runtime wrapper, lower a
#    `.push(...)` method call to MIR, JIT it, assert the Vec grew.

# 6. Branch off main:
git switch -c milestone-5
```

Good luck. Praxis now parses, type-checks, lowers, JIT-compiles, and executes
real programs end to end — `fn main() -> Int { fact(5) }` prints `120`, and
overflow returns to the host instead of crashing. The JIT has a heap to allocate
into, a fault protocol to unwind through, and a uniform calling convention to
build M5's collections on.
