# Where the decisions are written down

Almost every behaviour this book describes was decided once, in writing, before
it was implemented. Three kinds of document hold those decisions, and they answer
different questions.

**`praxis_technical_design.md`** is the contract. It is one 3,000-line
section-numbered document describing an intended language and its
implementation, and everything else in the repository refers back to it by
section number. It is also older than the code in places, and the code has moved.
Where the two disagree, the compiler wins.

**`docs/decisions/`** holds the ADRs — 129 of them, numbered to 132 — and the
rule they exist to serve is §20's first: *treat this document as the current
contract; record deliberate deviations in `docs/decisions/` before implementing
them.* An ADR is therefore not a summary of what happened. It is the record of a
place where the design document is no longer what the code does, written before
the code did it, with the reason attached.

**`docs/handovers/`** is the build log: one document per milestone or repair
round, saying what was built, what was measured, what was cut and what is left.
They are where a number came from — a benchmark figure, a profile share, an
instruction count — when an ADR quotes one.

This chapter is a map of the ADRs, so that when the book says a `for` loop
iterates a snapshot or a small `Int` is one shared object, you can find the
paragraph that decided it.

## How to read one

An ADR has a fixed shape — Context, one or more numbered Decisions, Consequences
— and no fixed length: the median is 121 lines and the longest is 1,105. Two
conventions are worth knowing before you open one.

**They are amended in place, and the amendment is at the top.** A later decision
that changes part of an earlier one edits the earlier file's header to say which
parts survive. So the top of
[ADR-019](../../../decisions/019-shadow-stack-spill.md) — the original
shadow-stack spill — carries two amendment notes, one from
[ADR-101](../../../decisions/101-the-shadow-stack-is-contiguous.md) retiring the
per-frame allocation and one from
[ADR-128](../../../decisions/128-a-shadow-slot-is-a-live-range-not-a-name.md)
retiring one-slot-per-local, and the body below them is deliberately left as
written. The reasoning that was superseded is kept, because it is what a future
reader will be tempted by.

**A number can be reserved before its decision is written.** Ten packages in one
round independently named their ADR `114-*.md`, and in ten separate worktrees
that is a silent overwrite rather than a merge conflict. So numbers are now
assigned in advance and nobody picks their own. An entry in the index that still
carries a status word — *owed*, *reserved* — was never written, and that is how
to tell.

Three numbers have no document. **121** and **123** were reserved and declined,
and [the index](../../../decisions/README.md) says so. **081** was never
assigned, never written and is mentioned nowhere; it is a gap and nothing more.

The index in `docs/decisions/README.md` is the authoritative list and is longer
than the map below in one respect: it carries the reserved entries too.

## Project and process

- [**001**](../../../decisions/001-snapshot-testing-insta.md) — snapshot tests use `insta`.
- [**002**](../../../decisions/002-ci-via-just.md) — CI is `just ci`, with a thin GitHub Actions wrapper around it.
- [**006**](../../../decisions/006-m1-fuzz-via-proptest.md) — the parser's never-panic gate is a `proptest` property, not a fuzzer binary.
- [**051**](../../../decisions/051-the-diagnostic-code-allocation.md) — how diagnostic numbers are allocated: which block means user error, internal error, member error, match error.

## The front end

- [**003**](../../../decisions/003-lossless-tree-uses-rowan.md) — the lossless syntax tree is `rowan`; Praxis owns the syntax kinds and the typed wrappers.
- [**004**](../../../decisions/004-parser-technique.md) — the parser is hand-written recursive descent with a Pratt loop, and recovery is local and explicit.
- [**005**](../../../decisions/005-formatter-lives-in-praxis-parser.md) — the formatter lives beside the parser and drives off the tree.
- [**009**](../../../decisions/009-m2-typed-ast-wrappers.md) — the typed AST is thin wrappers over rowan nodes, copying nothing.
- [**049**](../../../decisions/049-the-wildcard-binds-nothing-and-a-newline-ends-a-statement.md) — `_` binds nothing, and a newline ends a statement but never an unfinished expression.
- [**050**](../../../decisions/050-record-literals-are-legal-wherever-a-brace-cannot-be-a-block.md) — where `Name { … }` is a record literal and where it is a block.
- [**052**](../../../decisions/052-a-declaration-pass-seals-what-a-name-in-type-position-means.md) — a declaration pass fixes what every name in type position means before inference starts.
- [**065**](../../../decisions/065-a-type-constructors-brackets-are-type-arguments.md) — `Vec[Int]` is type arguments and `xs[i]` is a subscript, decided by what the name is.
- [**094**](../../../decisions/094-a-template-ends-at-the-line-it-opens-on.md) — an unterminated backtick template ends at its own line, so one missing tick does not eat the file.

## Types and inference

- [**007**](../../../decisions/007-type-representation-interning.md) — a type is an index into an interned arena.
- [**008**](../../../decisions/008-let-generalization-levels.md) — generalization uses Pottier-style binding levels rather than scanning the environment.
- [**046**](../../../decisions/046-sealed-type-handles-and-validated-constructors.md) — only the arena mints a `Type`, and each constructor validates its own arguments.
- [**047**](../../../decisions/047-scheme-owned-binders-and-the-level-newtype.md) — a scheme owns its binders, a level can only be lowered, and a declaration group predeclares its signatures so mutual recursion works.
- [**048**](../../../decisions/048-nominal-identity-is-a-definition-applied-to-arguments.md) — a nominal type is one definition applied to arguments, and that pair is its identity.
- [**024**](../../../decisions/024-provisional-structural-records.md) — the first, provisional structural record, since superseded in the type system by ADR-025.
- [**025**](../../../decisions/025-typedata-record-enum-defid.md) — records and enums are represented through a def-id indirection rather than inline.
- [**054**](../../../decisions/054-lowering-reads-the-type-inference-recorded.md) — lowering reads what inference recorded and never re-unifies; a specialization substitutes the scheme's own binders.
- [**057**](../../../decisions/057-a-capability-requirement-rides-on-the-scheme-that-quantified-it.md) — a capability requirement travels on the scheme that quantified the variable, and a map key must be hashable *and* immutable.
- [**062**](../../../decisions/062-an-iterated-parameter-is-generic-in-the-iterable-and-not-its-element.md) — a parameter you iterate is generic in the container and monomorphic in the element.
- [**063**](../../../decisions/063-a-self-referring-type-declaration-is-reported.md) — a type that refers to itself is reported once, and declarations behind it are not.
- [**093**](../../../decisions/093-a-method-that-cannot-resolve-is-reported-at-check.md) — an unresolvable method is a `check`-time error with exactly one emitter, and it is inference's.
- [**137**](../../../decisions/137-a-deferred-receiver-resolves-in-rounds-and-the-channel-runs-to-a-fixpoint.md) — a deferred receiver resolves in rounds, so the constraint channel discharges to a fixpoint rather than once.
- [**055**](../../../decisions/055-exhaustiveness-and-reachability-are-one-usefulness-question.md) — exhaustiveness and reachability are the same question asked at every sub-position.
- [**130**](../../../decisions/130-a-matchs-coverage-is-analysis-answer-and-the-pattern-is-built-once.md) — coverage is computed during analysis, so a non-exhaustive `match` fails `praxis check`.

## The standard library and the catalog

- [**010**](../../../decisions/010-method-catalog-bridge-and-m5-deferral.md) — the method catalog as the bridge between inference and the runtime, before dispatch existed.
- [**020**](../../../decisions/020-method-dispatch-and-collections.md) — `.method()` dispatch goes through the built-in catalog; there is no user-defined method.
- [**056**](../../../decisions/056-the-prelude-control-names-are-real-functions.md) — `panic`, `assert` and `dbg` are ordinary functions with ordinary types and a real fault.
- [**058**](../../../decisions/058-the-numeric-prelude-helpers-are-int-functions.md) — `abs`, `gcd`, `clamp` and the rest are monomorphic `Int` functions.
- [**060**](../../../decisions/060-the-graph-helpers-are-closure-driven-walks.md) — `bfs`, `dijkstra`, `a_star` and friends are closure-driven walks whose state is a value.
- [**076**](../../../decisions/076-absence-is-an-option-and-an-empty-min-is-a-fault.md) — absence is `Option`, but `min` of an empty collection is a fault.
- [**082**](../../../decisions/082-find-answers-the-element-and-a-miss-is-none.md) — `find` answers the element, `position` the index, and a miss is `None`.
- [**089**](../../../decisions/089-a-name-has-one-signature.md) — a name has exactly one signature, so a wrong argument count is its own diagnostic.

## Language semantics

- [**026**](../../../decisions/026-structural-equality-hashing.md) — equality and hashing are structural, driven by descriptors, with an internal capability check.
- [**027**](../../../decisions/027-closures.md) — the closure calling convention, capture analysis, and the cell a mutated capture lives in.
- [**028**](../../../decisions/028-collections-and-sequence-pipelines.md) — the collection set, the dynamic map key, and the shape of a sequence pipeline.
- [**037**](../../../decisions/037-float-implementation.md) — how `Float` rides a uniform `i64` channel that is not its own machine type.
- [**045**](../../../decisions/045-ordering-semantics-and-the-compare-callback.md) — ordering is IEEE in the source language and total inside a container, and the two are reconciled by one callback (which ADR-138 then populated on every key type).
- [**053**](../../../decisions/053-a-loop-is-the-value-its-breaks-carry.md) — `loop` is an expression whose value its `break`s carry; `while` and `for` are not.
- [**059**](../../../decisions/059-a-range-is-a-value-and-a-descending-one-is-empty.md) — a range is a first-class value, `..` is half-open, and a descending range is empty rather than reversed (ADR-145 added the countdown's spelling without reopening that).
- [**061**](../../../decisions/061-a-fn-name-in-value-position-is-a-closure.md) — naming a `fn` without calling it produces a closure over an adapter.
- [**066**](../../../decisions/066-a-for-iterates-a-snapshot.md) — `for` iterates a snapshot, and the snapshot is where iteration order is decided (ADR-138 changed what that order is).
- [**138**](../../../decisions/138-a-container-orders-by-the-value-and-not-by-its-printing.md) — a `Map`, `Set` or `Counter` walks and prints its keys in the value's own order, not in the order they render.
- [**139**](../../../decisions/139-a-pattern-name-is-a-name-in-the-frame.md) — a name a pattern introduces is a binding in the crash snapshot like any other, and a slot no pattern names is a temp.
- [**141**](../../../decisions/141-a-character-is-one-token-and-a-literal-is-a-load.md) — `'#'` is one token, `''`/`'ab'` are lex errors, and an ASCII character literal is two loads.
- [**143**](../../../decisions/143-the-to-text-family-is-int-float-and-char.md) — `Int`, `Float` and `Char` each render through the same writer `out` uses, so the two cannot disagree.
- [**144**](../../../decisions/144-a-sequence-of-text-joins-and-a-sequence-of-char-becomes-one.md) — `join` is one generic row bounded to `Text` items, and a sequence of `Char` becomes a line under a different name.
- [**145**](../../../decisions/145-a-reversal-needs-the-whole-sequence-so-it-is-a-barrier.md) — reversal cannot answer its first element until it has seen the last, so it is a barrier and not a fused stage.
- [**146**](../../../decisions/146-a-collection-constructors-arity-is-its-shape.md) — a collection constructor's argument *count* selects its shape, so `Vec(n, fill)` and `Grid(w, h, fill)` sit beside the nullary forms; a closed two-row narrowing of ADR-089, and the only arity overload in the language.
- [**147**](../../../decisions/147-a-hole-renders-anything-because-the-program-wrote-the-hole.md) — `"{v}"` renders any value through the printer `out` uses, and a hole's expression is a real subtree so a name in one is a closure capture; settles ADR-143 decision 4's open question without reopening ADR-085 decision 2, which still makes `"n = " + n` a `Y001`.
- [**067**](../../../decisions/067-a-files-top-level-statements-are-its-program.md) — a file's top-level statements are its program; `fn main` is the fallback.
- [**068**](../../../decisions/068-a-function-does-not-capture.md) — a named function captures nothing, and the diagnostic for assuming otherwise is `N007` (which drops its closure suggestion when the function is recursive).
- [**069**](../../../decisions/069-a-record-and-a-tuple-are-patterns-with-one-constructor.md) — records and tuples are patterns, each with one constructor form.
- [**064**](../../../decisions/064-a-subscript-is-a-catalog-row.md) — `xs[i]` is a catalog row like any method, and reading is a different row from storing.
- [**070**](../../../decisions/070-an-updating-store-is-a-row-with-a-contextual-operator.md) — `xs[i] += 1` is a catalog row, and which operator it means is decided by context.
- [**074**](../../../decisions/074-an-enum-value-records-which-enum-type-it-is.md) — an enum value carries which enum it belongs to, so two enums' variants are not confusable.
- [**077**](../../../decisions/077-a-zero-argument-accessor-is-a-call-and-a-bare-dot-name-is-a-field.md) — `.len()` is a call and `.x` is a field; the parentheses are the difference.
- [**083**](../../../decisions/083-a-float-prints-as-a-float.md) — a `Float` prints in a form that reads back as the same value, `-0.0` included.
- [**085**](../../../decisions/085-text-concatenation-is-plus-and-nothing-else-is.md) — `Text + Text` concatenates and no other operator applies to `Text`.
- [**086**](../../../decisions/086-a-text-subscript-answers-a-char.md) — `t[i]` answers a `Char`, and two conversions are the whole `Char` surface.
- [**091**](../../../decisions/091-a-variant-patterns-enum-is-the-scrutinees.md) — a variant pattern's enum comes from the scrutinee, and a record pattern needs no head.
- [**099**](../../../decisions/099-a-list-literal-is-a-vec-and-a-text-is-iterable.md) — `[a, b]` is a `Vec` literal, and `Text` is the eleventh iterable.
- [**071**](../../../decisions/071-a-pipeline-chain-is-nested-and-each-stage-counts-its-own-input.md) — a pipeline chain is a nested structure rather than a flat list, so `flat_map` cannot reach a stage that could not handle it.
- [**124**](../../../decisions/124-a-field-and-a-sequence-element-are-places.md) — a field and a sequence element are assignable places, and a store replaces what is there.
- [**125**](../../../decisions/125-a-binding-is-a-binding-and-the-compiler-decides-its-storage.md) — `let` is gone; `var` is the one binding form and every binding is assignable.
- [**126**](../../../decisions/126-a-pipeline-materializes-and-collect-named-a-step-it-takes-anyway.md) — a pipeline materializes on its own, so there is no `.collect()`.
- [**127**](../../../decisions/127-a-pipelines-source-is-the-for-loops-and-a-collection-converts-by-naming-what-it-becomes.md) — anything a `for` can iterate can start a pipeline, and a collection converts by naming what it becomes.
- [**031**](../../../decisions/031-fixed-width-deferred.md) — the fixed-width diagram helper was evaluated against the corpus and deferred.

## The input parser

- [**023**](../../../decisions/023-input-parser-dsl.md) — the DSL is three layers — compile-time crate, HIR bridge, runtime interpreter — each strictly downstream of the last.
- [**030**](../../../decisions/030-matrix-is-grid.md) — `matrix(P)` produces a `Grid[T]`, closing an open question in §21.
- [**072**](../../../decisions/072-a-template-capture-body-is-a-parser-expression.md) — a `{name:body}` capture body is a full parser expression, parsed by the scanner and not handed back to the language parser.
- [**073**](../../../decisions/073-a-constructor-call-is-a-shape-checked-before-it-is-built.md) — a constructor call's shape is validated before anything is constructed.
- [**140**](../../../decisions/140-a-counted-repeated-is-bounded-so-something-can-follow-it.md) — `repeated(P, N)` takes exactly N sections, so it is bounded and something can follow it.
- [**078**](../../../decisions/078-a-parser-position-is-absolute-and-a-region-only-narrows.md) — a parser position is absolute in the input, a region only narrows, and whether leftovers are an error is the parent's decision.
- [**079**](../../../decisions/079-a-grid-cell-is-what-its-cell-parser-reads.md) — a grid cell is whatever its cell parser reads, a capture is non-greedy, and a collection's element type is its child's.
- [**084**](../../../decisions/084-a-template-is-a-parser-expression-everywhere-or-nowhere.md) — a backtick template is a parser expression everywhere, so writing one in value position is a diagnostic rather than a string.
- [**087**](../../../decisions/087-empty-input-is-input-and-no-input-is-a-host-state.md) — empty input is input; "no input at all" is a state of the host, not of the program.
- [**090**](../../../decisions/090-a-block-item-is-offered-its-own-lines.md) — a `block` item is offered its own lines, and the window narrows rather than bounds.
- [**092**](../../../decisions/092-a-templates-shape-is-read-from-its-parts.md) — a template's result shape is derived from its parts in one place, and two or more anonymous captures make a tuple.
- [**111**](../../../decisions/111-a-text-literals-bytes-are-the-compilers-promise.md) — a literal's bytes are the compiler's promise and the input's are the host's, so only the host's are validated.

## MIR and code generation

- [**014**](../../../decisions/014-typed-hir-tree-as-lowering-boundary.md) — a typed HIR tree is the boundary between the front end and MIR.
- [**015**](../../../decisions/015-mir-shape-non-ssa-slots.md) — MIR is slot-based and not SSA; Cranelift builds the SSA.
- [**016**](../../../decisions/016-mir-liveness-and-roots.md) — backward liveness computes the minimal root set at each safepoint.
- [**018**](../../../decisions/018-monomorphization-deferred.md) — monomorphization deferred. **Superseded**: it landed in M7.
- [**044**](../../../decisions/044-two-slot-sets-and-the-mir-verifier.md) — the collector's root set and the debugger's view are two different sets, and a verifier keeps the first honest.
- [**088**](../../../decisions/088-a-faulting-instruction-is-observed-by-the-next-one.md) — a fault is observed by the instruction after the one that raised it, and only a faulting instruction may raise.
- [**102**](../../../decisions/102-a-check-is-a-branch-not-a-call.md) — a type proof before a scalar load is an inline branch, and the check itself is not removed.
- [**108**](../../../decisions/108-the-builder-holds-the-preheader-so-the-pass-is-not-needed.md) — a loop-invariant box is hoisted by the builder, which already knows the preheader, rather than by a pass.
- [**110**](../../../decisions/110-a-pure-wrapper-is-a-load-not-a-call.md) — allocating `Unit` or `Bool` is a load and a `select`, because neither has allocated for some time.
- [**116**](../../../decisions/116-a-descriptors-address-is-the-runtimes-and-the-compiler-names-a-slot.md) — descriptor addresses live in the runtime context and the compiler names a slot, not an address.
- [**117**](../../../decisions/117-a-raise-that-branches-is-its-own-observation.md) — a fault raised by a branch observes itself, and only checked `Int` arithmetic can be one.
- [**118**](../../../decisions/118-a-vecs-three-words-are-the-compilers-to-read.md) — a `Vec`'s pointer/length/capacity are a `#[repr(C)]` fact generated code may read; a `VecDeque`'s are not.
- [**120**](../../../decisions/120-a-box-with-one-reader-in-its-own-block-is-not-a-box.md) — a box read once in its own block is deleted, and what it cost the debugger is given back.
- [**122**](../../../decisions/122-a-descriptor-the-compiler-wrote-is-provable-and-a-parameter-is-not.md) — a static analysis says which descriptor a slot provably holds, and the verifier refuses a load that contradicts it.
- [**029**](../../../decisions/029-pipeline-fusion.md) — a pipeline chain is recognized and emitted as one fused loop with no intermediate collection.
- [**042**](../../../decisions/042-total-type-descriptor-bridge.md) — one total bridge between a static type and a runtime descriptor, and the JIT refuses rather than mislabelling.
- [**043**](../../../decisions/043-generation-arena-and-the-teardown-proof.md) — JIT and parser metadata belongs to a reclaimable arena, and reclaiming one requires proof the heap is gone.

## The runtime, the ABI and the collector

- [**011**](../../../decisions/011-gc-bumpalo-mark-sweep.md) — precise, non-moving mark-and-sweep. The arena and side registry it chose are gone; the character is not.
- [**012**](../../../decisions/012-root-tracking-explicit-frames.md) — explicit host-side root frames, and the trait seam the compiler-managed shadow stack later plugged into.
- [**013**](../../../decisions/013-m3-descriptors-scalars-and-vec.md) — the first descriptors: six scalars and a minimal `Vec[T]`.
- [**017**](../../../decisions/017-runtime-abi-wrappers-and-fault-protocol.md) — every runtime wrapper is `extern "C"`, never panics, and reports by setting a pending fault.
- [**019**](../../../decisions/019-shadow-stack-spill.md) — the compiler spills live roots into a frame before each safepoint. Amended twice; read the header.
- [**022**](../../../decisions/022-source-slice-text.md) — a `Text` may be a zero-copy slice of another `Text`, which is what makes input parsing allocation-free.
- [**038**](../../../decisions/038-derived-builtin-type-identity.md) — a built-in's type id is derived from its enum, and descriptors are `static` because pointer identity is what is compared.
- [**039**](../../../decisions/039-gc-header-layout-authority-and-heap-provenance.md) — one function computes the object layout and the header records it; every allocation carries its heap's identity.
- [**040**](../../../decisions/040-safepoint-token-and-the-unpaced-back-door.md) — allocation requires a token that only the pacing function mints, so allocating without pacing has no spelling.
- [**041**](../../../decisions/041-bounded-extents-fault-instead-of-aborting.md) — a size the host cannot serve is a fault with a code, not an abort.
- [**080**](../../../decisions/080-totality-is-the-contract-and-catch-unwind-is-the-proof.md) — totality is the contract at the ABI boundary and `catch_unwind` is what proves it.
- [**075**](../../../decisions/075-the-two-owed-fault-kinds-are-paid.md) — the last two missing fault kinds, and why the third debt was settled differently.
- [**100**](../../../decisions/100-a-small-int-is-one-object-and-a-literal-is-a-load.md) — every `Int` from −256 to 1024 is one shared object, and a literal is a load.
- [**107**](../../../decisions/107-a-small-char-is-one-object-and-there-is-no-char-literal.md) — every code point below 128 is one shared object (its second clause, that there is no character literal syntax, is superseded by ADR-141).
- [**101**](../../../decisions/101-the-shadow-stack-is-contiguous.md) — the shadow stack is one region and a frame is a run of slots in it, claimed inline with no call.
- [**103**](../../../decisions/103-a-page-owns-the-storage-and-the-liveness.md) — storage is a size-class page, liveness is a bitmap on it, and the side registry is gone.
- [**105**](../../../decisions/105-the-recursion-guard-spends-a-byte-budget.md) — the recursion guard charges each frame by its shape in bytes rather than counting calls.
- [**109**](../../../decisions/109-pages-stay-segregated-by-size-class.md) — pages stay segregated by size class rather than by descriptor, and the header shrinks to 16 bytes instead.
- [**112**](../../../decisions/112-the-pacer-has-a-ceiling-and-the-live-set-may-exceed-it.md) — the collection threshold is bounded, and only the measured live set may exceed the bound.
- [**113**](../../../decisions/113-an-int-box-is-a-table-read-behind-a-pacing-branch.md) — generated code inlines the pacing test and the interned-`Int` probe behind it.
- [**114**](../../../decisions/114-the-native-roots-are-one-store-and-only-their-depth-is-bounded.md) — native code's roots live in one growable store whose depth, not size, is what is bounded.
- [**115**](../../../decisions/115-a-text-counts-itself-once-and-the-count-is-the-licence.md) — a `Text` counts its scalars once, and that count is what licenses byte indexing.
- [**119**](../../../decisions/119-generated-code-claims-the-block-and-nothing-between-can-collect.md) — generated code claims an allocation block inline, and the safety argument is that nothing between the pacing branch and the last store can collect.
- [**129**](../../../decisions/129-the-ceiling-is-worth-what-a-collection-costs.md) — the pacing ceiling is a bet on what a collection costs, and two unrelated changes moved the knee from 64 MiB to 4 MiB.

## The crash debugger

- [**021**](../../../decisions/021-debug-frame-metadata.md) — the debug frame's layout, and how a shadowed binding is told apart from the one shadowing it.
- [**032**](../../../decisions/032-debugger-expr-main-heap.md) — a `p EXPR` allocates on the main heap; there is no second generation.
- [**033**](../../../decisions/033-crash-snapshot-rooting.md) — the snapshot deep-copies the frame chain at the first fault epilogue, and roots through it.
- [**034**](../../../decisions/034-read-only-purity-gate.md) — a debugger expression must be provably read-only before it runs.
- [**035**](../../../decisions/035-static-type-id-and-source-span-threading.md) — the full static type and the source span are threaded into the debug frame, which is what lets a local render with its type.
- [**036**](../../../decisions/036-synthetic-p-expr-function.md) — `p EXPR` compiles and runs a synthesized function against the selected frame.
- [**104**](../../../decisions/104-the-debugger-view-is-written-once-per-value.md) — the debugger's view is written once at a value's definition, and a frame is two claims on two stacks.
- [**106**](../../../decisions/106-the-debug-values-are-the-collectors-one-weak-arm.md) — the debugger's slots are weak: scanned after every sweep and nulled, never traced.
- [**128**](../../../decisions/128-a-shadow-slot-is-a-live-range-not-a-name.md) — a shadow slot is a live range and slots are shared by colouring; the debugger's slot is the one that keeps a name.

## The language server

- [**095**](../../../decisions/095-the-language-server-is-a-synchronous-stdio-loop.md) — the server is a synchronous, single-threaded stdio loop with no async runtime.
- [**096**](../../../decisions/096-positions-convert-at-the-protocol-boundary.md) — UTF-16 positions are converted at the protocol edge; everything inside stays byte-based.
- [**097**](../../../decisions/097-the-shared-query-layer-lives-in-praxis-lsp.md) — the shared front-end query layer lives in the LSP crate, and `praxis check` routes through it.
- [**098**](../../../decisions/098-the-parser-ast-is-retained-by-inference.md) — inference retains the input-parser AST and its per-node types, so the editor needs no second scanner.
- [**131**](../../../decisions/131-a-rename-is-safe-when-re-resolution-is-unchanged.md) — a rename is safe when re-resolving the edited text gives the same answers, which is checked rather than enumerated.
- [**132**](../../../decisions/132-a-code-action-is-a-diagnostics-machine-applicable-suggestion.md) — a quick fix is a property of the diagnostic, so the code-action module knows about no particular diagnostic.

## Finding the reasoning for anything else

The map above is a starting point, not an index of every behaviour. When the
question is "why does this do that", the reliable route is through the code
rather than through the documents:

1. Find the behaviour in the crate that owns it (the table in
   [the pipeline chapter](./pipeline.md) says which).
2. Read the doc comment. Praxis's module and item docs cite the design section
   and the ADR number that decided them, usually in the first paragraph.
3. Open the ADR. Read its header first for amendments, then its Context — which
   is generally the defect or the question that forced the decision, and is
   usually the part you wanted.

If the behaviour has no ADR, that is itself an answer: it means the design
document already specified it and nothing deviated. Search
`praxis_technical_design.md` by section number, and remember that where it and
the compiler disagree, the compiler is right and the document is old.
