# ADR-023: Input-parser DSL — three-layer compile/runtime split

**Date:** 2026-07-23 · **Status:** accepted

## Context

§7 specifies a unified input-parser DSL with its own typed AST. Rule §7.9 is
explicit: **"Do not lower it immediately into string splitting calls."** The DSL
must express `read`/`parse` expressions, backtick templates, atomic parsers
(int, char, etc.), and structural constructors (`lines`, `sections`, `csv`, `ws`,
`grid`, …). Two §7 sub-requirements make this harder than a normal lowering pass:

- §7.8 demands **compile-time type synthesis** — the parser's result type is
  derived *statically* from its shape, before any input is read.
- The DSL also needs a **runtime interpreter** that walks the plan against input
  bytes and allocates GC results.

The architectural risk is a dependency cycle: type synthesis lives in the
compiler crates (`praxis-types`), and the interpreter lives in
`praxis-runtime`, but the runtime must see the *same* plan representation the
compiler produced. A naive design has `praxis-input-parser` depend on
`praxis-types` for synthesis *and* on `praxis-runtime` for execution — or worse,
`praxis-runtime` reaching back into `praxis-hir` to read plans.

## Decision

Split the DSL into three layers, each a strictly downstream dependency of the
previous:

1. **`praxis-input-parser`** — the compile-time DSL crate. It owns:
   - `ParserAst` — the typed tree per §7.9 (`Atomic`, `Template`, `Lines`,
     `Sections`, `Csv`, `Ws`, `Grid`, …).
   - `scan_template` — re-scans the interior of a backtick template into
     `TemplatePart`s.
   - `validate` — static checks (arity, well-formedness) returning
     `ValidationError`.
   - `synthesize` — the §7.8 type derivation over `praxis-types`.
   - `ParserPlan` — the flat `#[repr(C)]` runtime node arena (`PlanNode` +
     `lower_to_plan`), plus the **plan slab** (`register_plan` / `get_plan`)
     that holds plans for the process lifetime.

2. **`praxis-hir` `parser_lower.rs`** — the bridge from the lossless syntax tree
   into the DSL. It converts rowan `ParserExpr` nodes → `ParserAst`, then runs
   `validate`, `synthesize` (for inference), `lower_to_plan`, and
   `register_plan` (for lowering). `TypedExpr` gains `Read { plan_index, ty }`
   and `Parse { plan_index, ty }` variants carrying the slab index.

3. **`praxis-runtime` `parser.rs`** — the interpreter. It walks a `ParserPlan`
   against the input bytes (`Text` `GcRef`), allocating GC results (`Int`,
   `Char`, source-slice `Text`, `Vec`, `Grid`, `Record`). The host reaches it
   through the `praxis_run_parser(ctx, plan_index_gc, input_gc)` ABI wrapper.

## Reason

- **Dependency direction is acyclic:**
  `praxis-input-parser → praxis-types` (for synthesis) and
  `praxis-runtime → praxis-input-parser` (for plan types + slab lookup).
  The break in the would-be cycle is that **record schemas are built at runtime,
  not stored in the plan**: the plan stores only field names as `&'static str`,
  so the plan types never depend on the runtime descriptor types. The plan module
  exists precisely "without creating a dependency cycle."

- **The plan is leaked to `&'static` and indexed by `u32`.** This mirrors how the
  JIT already leaks function-name strings, and it lets the plan arena use plain
  `&'static [PlanNode]` slices with child references as indices — no lifetime
  threading through generated code. `register_plan` returns the index; `get_plan`
  is the runtime's only lookup.

- **The MIR passes the index as a boxed `Int` `GcRef`, matching the uniform ABI**
  (§10.2: every value crosses the boundary as a `GcRef`). The interpreter recovers
  it via `int_payload(plan_index_gc)`, so no special-cased calling convention or
  extra symbol table is needed — `praxis_run_parser(ctx, plan_index_gc,
  input_gc)` is a normal runtime call.

## Consequences

- **Clean separation of compile-time and runtime concerns.** Type checking,
  validation, and §7.8 synthesis are pure host-side work; execution is pure
  interpretation. A parser that fails validation never produces a plan.
- **Plans live for the process lifetime.** The leak is acceptable for a JIT:
  plans are tied to compiled functions that are themselves not reclaimed. The
  slab is a `Mutex<Vec<PlanEntry>>` guarded by a `Send + Sync` wrapper asserting
  the plan's raw pointers only reference process-static descriptor data.
- **The interpreter is pure Rust — no JIT codegen for parsing.** Plans are
  interpreted, so the parsing hot path avoids Cranelift entirely; this keeps M6's
  surface area small and debuggable.
- **M9 extends the constructor set.** `block`, `choice`, and `scan` constructors
  land in M9 by adding a `ParserAst` arm, a `PlanNode` arm, a `walk_*` interpreter
  case, and one catalog entry — the layering is designed to absorb them without
  touching the ABI or the slab.
