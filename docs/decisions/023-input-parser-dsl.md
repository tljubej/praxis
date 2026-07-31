# ADR-023: Input-parser DSL — three-layer compile/runtime split

**Date:** 2026-07-23 · **Status:** accepted · **Amended:** 2026-07-31 (S19)

> ## Amendments (2026-07-31, stage S19)
>
> The three-layer split is unchanged and so is the dependency direction. Four
> claims below are not true any more, and two §7.4 deviations are recorded here
> because this is the ADR that describes what the DSL crate owns.
>
> 1. **`scan_template` parses capture bodies.** The layer-1 bullet says it
>    "re-scans the interior of a backtick template into `TemplatePart`s" and the
>    module doc used to add that it "only classifies template structure — it does
>    not parse capture bodies", with the parser-expression parser in
>    `praxis-parser` feeding the capture interior back through the ordinary
>    grammar. That never happened: the body was thrown away and the HIR guessed
>    it back by rescanning the template. Under **D10** a capture body is a full
>    parser expression, and it is parsed *inside* `praxis-input-parser` — which is
>    what the dependency direction this ADR fixes requires. See **ADR-072**.
>
> 2. **The plan is not `#[repr(C)]` and plans do not live for the process
>    lifetime.** "The flat `#[repr(C)]` runtime node arena" and "holds plans for
>    the process lifetime" were superseded by S8 (IP-12): a `CompiledPlan` owns a
>    `bumpalo` arena, registration is bounded and refuses cleanly, `PlanId` is a
>    `NonZeroU32`, and `retire_all_plans` reclaims. `plan.rs`'s own module doc is
>    the authority; the same stale claim in
>    `crates/praxis-runtime/src/parser.rs` has been corrected. See also ADR-043.
>
> 3. **`validate` is not the only static check, and `check_constructor_arity` is
>    gone.** A constructor call is checked against §7.5's *shape* before anything
>    is built, by `check_call`/`build_call`, which the rowan bridge and the
>    capture-body parser share. See **ADR-073**.
>
> 4. **The MIR immediate is a `PlanId`, not a boxed `Int` index.** "The MIR
>    passes the index as a boxed `Int` `GcRef`" predates S8's `PlanId`.
>
> ### §7.4 deviations, recorded (IP-11)
>
> - **`uint` synthesizes `Int`, not `ScalarType::UInt`.** `UInt` is reserved and
>   has no runtime object: `praxis_repr::builtin_for_type` answers
>   `NoRuntimeRepr` for it (pinned by
>   `a_type_with_no_runtime_object_has_no_descriptor`), and under **D9** a JIT
>   compile fails when a descriptor is missing — so a `uint` capture typed `UInt`
>   would make every program containing one fail to compile. §7.4's
>   non-negativity is enforced by the **parse rule**: a leading `-` is not a
>   `uint`.
> - **`identifier` uses §4.1's Unicode identifier class**, not §7.4's
>   "ASCII-like identifier syntax by default". F3 gave the workspace one
>   character class; a parser that accepted fewer names than the language itself
>   declares would refuse identifiers a Praxis program can write.
> - **`byte` means a decimal integer in `0..=255`**, not a raw input byte: a raw
>   byte cannot be re-sliced as `Text` without breaking the UTF-8 invariant every
>   source-slice `Text` depends on.
>
> ### One more crate boundary
>
> `praxis-runtime` names `praxis-syntax` directly now (it was already an indirect
> dependency), because §7.4's `identifier` atomic parses with §4.1's character
> class and a second copy of that rule is what F3 exists to prevent. The
> workspace's one text-literal decoder moved to `praxis_syntax::literal` for the
> same reason, and so did the one rule for **where a backtick template ends**
> (`praxis_syntax::template`): the lexer decides the extent of the token and the
> scanner decides the extent of every template inside it, and while those were
> two implementations of one rule they disagreed about string literals and about
> the nesting bound. `praxis-input-parser` still does **not** depend on
> `praxis-parser`, and `praxis-parser` does not depend on `praxis-input-parser` —
> which is why the shared rule sits *under* both rather than in either.

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
