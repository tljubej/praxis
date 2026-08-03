# Architectural decisions

Per the Praxis technical design (§20, rule 1), the design document is the
current contract and deliberate deviations are recorded here before
implementing them. Each entry is short: context, decision, and the reason.

Entries are numbered and dated. Add new decisions at the bottom.

**A number can be reserved before its decision is written.** Ten packages in the
[handover-26](../handovers/26-ten-packages-six-waves-and-the-five-things-25-got-wrong.md)
round independently named their ADR `114-*.md`, and in ten separate worktrees
that is a *silent overwrite* rather than a merge conflict — two unrelated
documents at one path, and the merge keeps whichever it saw last. So `114..123`
below are assigned in advance, one per package, and nobody picks their own
(§2, §7 trap 1).

A reserved entry is a placeholder: it carries a **status word** where its title
belongs and points at `NNN-slug.md`, which is not a file. Writing the decision is
then a **one-line edit** — real title, real filename, status word gone — rather
than an append that two branches would both perform at the same offset. A line
that still has a status word was never written, and that is how to tell six
months from now.

There are two status words and the difference is the point: ***owed*** means the
package is planned this round and is expected to write the decision, while
***reserved*** means handover 26 defers or declines the package, so the number
may stay unwritten forever. A reserved number is still spent — handover 26's
prose already refers to these numbers by name, so recycling one would make an
existing document wrong.

A status word can move, and one already has:
[handover 27](../handovers/27-the-five-gates-and-what-26-got-wrong.md) §5 splits
W11 into a safety half worth building now and a backend half with no honest slot
before the round's last gate, so **122 became *owed*** — for the analysis and the
verifier rule only. Its decision must not claim the elision.

- [ADR-001: Snapshot testing library is `insta`](./001-snapshot-testing-insta.md)
- [ADR-002: CI runs `just ci`, with a minimal GitHub Actions wrapper](./002-ci-via-just.md)
- [ADR-003: Lossless syntax tree uses the `rowan` crate](./003-lossless-tree-uses-rowan.md)
- [ADR-004: Hand-written recursive-descent parser with Pratt climbing](./004-parser-technique.md)
- [ADR-005: Formatter skeleton lives in `praxis-parser`](./005-formatter-lives-in-praxis-parser.md)
- [ADR-006: Milestone 1 fuzz gate is a `proptest` property test](./006-m1-fuzz-via-proptest.md)
- [ADR-007: Type representation is an interned arena](./007-type-representation-interning.md)
- [ADR-008: let-generalization uses Pottier-style binding levels](./008-let-generalization-levels.md)
- [ADR-009: Minimal typed AST wrappers over rowan nodes](./009-m2-typed-ast-wrappers.md)
- [ADR-010: Method catalog bridge in M2; `.method()` dispatch deferred to M5](./010-method-catalog-bridge-and-m5-deferral.md)
- [ADR-011: Precise non-moving mark-and-sweep over a Bumpalo arena + live-set registry](./011-gc-bumpalo-mark-sweep.md)
- [ADR-012: Explicit root frames for M3](./012-root-tracking-explicit-frames.md)
- [ADR-013: Scalar + Vec[T] descriptors in M3; other collection descriptors in M5](./013-m3-descriptors-scalars-and-vec.md)
- [ADR-014: A typed-HIR tree as the MIR lowering boundary](./014-typed-hir-tree-as-lowering-boundary.md)
- [ADR-015: MIR shape — non-SSA slots, transient scalars, Cranelift makes SSA](./015-mir-shape-non-ssa-slots.md)
- [ADR-016: MIR liveness and the per-safepoint root set](./016-mir-liveness-and-roots.md)
- [ADR-017: Runtime ABI wrappers and the no-panic fault protocol](./017-runtime-abi-wrappers-and-fault-protocol.md)
- [ADR-018: Monomorphization deferred — M4 is monomorphic](./018-monomorphization-deferred.md)
- [ADR-019: Compiler-managed shadow-stack spill](./019-shadow-stack-spill.md)
- [ADR-020: Method-call dispatch through the built-in catalog](./020-method-dispatch-and-collections.md)
- [ADR-021: Debug frame metadata and shadowed-symbol registration](./021-debug-frame-metadata.md)
- [ADR-022: Source-slice `Text` representation in M6](./022-source-slice-text.md)
- [ADR-023: Input-parser DSL architecture](./023-input-parser-dsl.md)
- [ADR-024: Provisional structural records ahead of M7](./024-provisional-structural-records.md)
- [ADR-025: TypeData record/enum via def-id indirection](./025-typedata-record-enum-defid.md)
- [ADR-026: Structural equality & hashing via descriptors + internal capability check](./026-structural-equality-hashing.md)
- [ADR-027: Closures — Approach B calling convention, capture analysis, VarCell](./027-closures.md)
- [ADR-028: Collections, DynamicKey, and the sequence pipeline (M8)](./028-collections-and-sequence-pipelines.md)
- [ADR-029: Pipeline fusion and the chain-recognition pass (M8-WS11)](./029-pipeline-fusion.md)
- [ADR-030: `matrix(P)` is `Grid[T]` (closes §21.1)](./030-matrix-is-grid.md)
- [ADR-031: Fixed-width diagram helper deferred](./031-fixed-width-deferred.md)
- [ADR-032: Debugger expressions allocate on the main GC heap (closes §21.8)](./032-debugger-expr-main-heap.md)
- [ADR-033: Crash snapshots root through `DebugLocal.value` copies (M10-WS3)](./033-crash-snapshot-rooting.md)
- [ADR-034: Read-only purity gate for debugger expressions (M10b-WS4)](./034-read-only-purity-gate.md)
- [ADR-035: Full static `Type` id + source span threaded into the debug frame (M10b-WS1)](./035-static-type-id-and-source-span-threading.md)
- [ADR-036: `p EXPR` evaluates a synthesized `__p_expr` function (M10b-WS4)](./036-synthetic-p-expr-function.md)
- [ADR-037: Float scalar implementation](./037-float-implementation.md)
- [ADR-038: Built-in type identity is derived; descriptors are `static`](./038-derived-builtin-type-identity.md)
- [ADR-039: The `GcHeader` owns the object layout; allocations carry their heap](./039-gc-header-layout-authority-and-heap-provenance.md)
- [ADR-040: A `Safepoint` token gates allocation, with one named unpaced route](./040-safepoint-token-and-the-unpaced-back-door.md)
- [ADR-041: A size the host cannot serve is a fault, not an abort](./041-bounded-extents-fault-instead-of-aborting.md)
- [ADR-042: One total bridge between `Type` and `TypeDescriptor`; the JIT refuses rather than mislabels](./042-total-type-descriptor-bridge.md)
- [ADR-043: JIT and parser metadata belongs to a reclaimable generation, gated by a teardown proof](./043-generation-arena-and-the-teardown-proof.md)
- [ADR-044: Two slot sets — the collector's roots and the debugger's view — and the MIR verifier](./044-two-slot-sets-and-the-mir-verifier.md)
- [ADR-045: Ordering is scalar, total in containers, and IEEE in the source language](./045-ordering-semantics-and-the-compare-callback.md)
- [ADR-046: A `Type` is minted by the arena, and a type constructor validates its own arguments](./046-sealed-type-handles-and-validated-constructors.md)
- [ADR-047: A scheme owns its binders, a level can only be lowered, and a declaration group predeclares its signatures](./047-scheme-owned-binders-and-the-level-newtype.md)
- [ADR-048: A nominal type is one definition applied to arguments, and `TypeKey` is its identity](./048-nominal-identity-is-a-definition-applied-to-arguments.md)
- [ADR-049: `_` is a wildcard that binds nothing, and a newline ends a statement but never an expression](./049-the-wildcard-binds-nothing-and-a-newline-ends-a-statement.md)
- [ADR-050: A record literal is legal wherever the brace cannot be a block](./050-record-literals-are-legal-wherever-a-brace-cannot-be-a-block.md)
- [ADR-051: The diagnostic-code allocation](./051-the-diagnostic-code-allocation.md)
- [ADR-052: A declaration pass seals what a name in type position means](./052-a-declaration-pass-seals-what-a-name-in-type-position-means.md)
- [ADR-053: A `loop` is the value its `break`s carry, and it is the only loop that has one](./053-a-loop-is-the-value-its-breaks-carry.md)
- [ADR-054: Lowering reads the type inference recorded, and a specialization substitutes the scheme's own binders](./054-lowering-reads-the-type-inference-recorded.md)
- [ADR-055: Exhaustiveness and reachability are one usefulness question, asked at every position a value has](./055-exhaustiveness-and-reachability-are-one-usefulness-question.md)
- [ADR-056: `panic`, `assert` and `dbg` are real functions with real types and a real fault](./056-the-prelude-control-names-are-real-functions.md)
- [ADR-057: A capability requirement rides on the scheme that quantified it, and a key is hashable *and* immutable](./057-a-capability-requirement-rides-on-the-scheme-that-quantified-it.md)
- [ADR-058: The numeric prelude helpers are monomorphic `Int` functions, and their arity is their wrapper's](./058-the-numeric-prelude-helpers-are-int-functions.md)
- [ADR-059: A range is a value, `..` is half-open, and a descending range is empty](./059-a-range-is-a-value-and-a-descending-one-is-empty.md)
- [ADR-060: The graph helpers are closure-driven walks, and their state is a value that can be remembered](./060-the-graph-helpers-are-closure-driven-walks.md)
- [ADR-061: A `fn` name in value position is a closure over an adapter](./061-a-fn-name-in-value-position-is-a-closure.md)
- [ADR-062: An iterated parameter is generic in the iterable and monomorphic in its element](./062-an-iterated-parameter-is-generic-in-the-iterable-and-not-its-element.md)
- [ADR-063: A self-referring type declaration is reported, and a declaration behind one is not](./063-a-self-referring-type-declaration-is-reported.md)
- [ADR-064: A subscript is a catalog row, and a store is a different row from a read](./064-a-subscript-is-a-catalog-row.md)
- [ADR-065: A type constructor's brackets are type arguments and every other name's are a subscript](./065-a-type-constructors-brackets-are-type-arguments.md)
- [ADR-066: A `for` iterates a snapshot, and the snapshot is where the order is decided](./066-a-for-iterates-a-snapshot.md)
- [ADR-067: A file's top-level statements are its program, and `fn main` is the fallback](./067-a-files-top-level-statements-are-its-program.md)
- [ADR-068: A function does not capture, and saying so is `N007`](./068-a-function-does-not-capture.md)
- [ADR-069: A record and a tuple are patterns, and each has one constructor](./069-a-record-and-a-tuple-are-patterns-with-one-constructor.md)
- [ADR-070: An updating store is a catalog row, and its operator is decided contextually](./070-an-updating-store-is-a-row-with-a-contextual-operator.md)
- [ADR-071: A pipeline chain is nested, and each stage counts its own input](./071-a-pipeline-chain-is-nested-and-each-stage-counts-its-own-input.md)
- [ADR-074: An enum value records which enum type it is](./074-an-enum-value-records-which-enum-type-it-is.md)
- [ADR-075: The two owed fault kinds are paid, and one of the three debts is settled differently](./075-the-two-owed-fault-kinds-are-paid.md)
- [ADR-076: Absence is an `Option`, and an empty `min` is a fault](./076-absence-is-an-option-and-an-empty-min-is-a-fault.md)
- [ADR-077: A zero-argument accessor is a call, and a bare `.name` is a field](./077-a-zero-argument-accessor-is-a-call-and-a-bare-dot-name-is-a-field.md)
- [ADR-072: A template capture body is a parser expression, and the scanner parses it](./072-a-template-capture-body-is-a-parser-expression.md)
- [ADR-073: A parser constructor call is a shape, checked before anything is built](./073-a-constructor-call-is-a-shape-checked-before-it-is-built.md)
- [ADR-078: A parser position is absolute, a region only narrows, and exhaustion is the parent's decision](./078-a-parser-position-is-absolute-and-a-region-only-narrows.md)
- [ADR-079: A grid cell is what its cell parser reads, a capture is non-greedy, and a collection's type is its child's](./079-a-grid-cell-is-what-its-cell-parser-reads.md)
- [ADR-080: Totality is the contract at the ABI boundary, and `catch_unwind` is the proof](./080-totality-is-the-contract-and-catch-unwind-is-the-proof.md)
- [ADR-082: `find` answers the element, `position` answers the index, and a miss is `None`](./082-find-answers-the-element-and-a-miss-is-none.md)
- [ADR-083: A `Float` prints as a `Float`](./083-a-float-prints-as-a-float.md)
- [ADR-084: A backtick template is a parser expression, so in value position it is a diagnostic](./084-a-template-is-a-parser-expression-everywhere-or-nowhere.md)
- [ADR-085: `Text + Text` is concatenation, and no other operator is defined for `Text`](./085-text-concatenation-is-plus-and-nothing-else-is.md)
- [ADR-086: A `Text` subscript answers a `Char`, and two conversions are the whole `Char` surface](./086-a-text-subscript-answers-a-char.md)
- [ADR-087: Empty input is input, and no input is a host state](./087-empty-input-is-input-and-no-input-is-a-host-state.md)
- [ADR-088: A faulting instruction is observed by the next one, and only a faulting instruction is](./088-a-faulting-instruction-is-observed-by-the-next-one.md)
- [ADR-090: A `block` item is offered its own lines, and the window is a narrowing rather than a bound](./090-a-block-item-is-offered-its-own-lines.md)
- [ADR-089: A name has one signature, so `assert` takes a condition and the arity mismatch gets a code](./089-a-name-has-one-signature.md)
- [ADR-091: A variant pattern's enum is the scrutinee's, and a record pattern needs no head](./091-a-variant-patterns-enum-is-the-scrutinees.md)
- [ADR-092: A template's shape is read from its parts, in one place, and there is no tuple node](./092-a-templates-shape-is-read-from-its-parts.md)
- [ADR-093: A method that cannot resolve is reported at `check` — one emitter for `Y110`, and it is inference's](./093-a-method-that-cannot-resolve-is-reported-at-check.md)
- [ADR-094: A backtick template ends at the line it opens on](./094-a-template-ends-at-the-line-it-opens-on.md)
- [ADR-095: The language server is a synchronous, single-threaded stdio loop](./095-the-language-server-is-a-synchronous-stdio-loop.md)
- [ADR-096: Positions convert at the protocol boundary; `LineMap` stays byte-based](./096-positions-convert-at-the-protocol-boundary.md)
- [ADR-097: The shared query layer lives in `praxis-lsp`, and `praxis check` routes through it](./097-the-shared-query-layer-lives-in-praxis-lsp.md)
- [ADR-098: The parser AST is retained by inference, and per-node types with it](./098-the-parser-ast-is-retained-by-inference.md)
- [ADR-099: A `[a, b]` is a `Vec` literal, and a `Text` is the eleventh iterable](./099-a-list-literal-is-a-vec-and-a-text-is-iterable.md)
- [ADR-100: A small `Int` is one object per value, and a literal is a load](./100-a-small-int-is-one-object-and-a-literal-is-a-load.md)
- [ADR-101: The shadow stack is one contiguous region, and the recursion limit is what keeps it in bounds](./101-the-shadow-stack-is-contiguous.md)
- [ADR-102: A check is a branch, not a call — and the check itself stays](./102-a-check-is-a-branch-not-a-call.md)
- [ADR-103: A page owns the storage and the liveness, and the registry is gone](./103-a-page-owns-the-storage-and-the-liveness.md)
- [ADR-104: The debugger's view is written once per value, and a frame is two slot-stack claims](./104-the-debugger-view-is-written-once-per-value.md)
- [ADR-105: The recursion guard spends a byte budget, and the budget is a value the host installs](./105-the-recursion-guard-spends-a-byte-budget.md)
- [ADR-106: The debug values are the collector's one weak arm](./106-the-debug-values-are-the-collectors-one-weak-arm.md)
- [ADR-107: A small `Char` is one object per code point, and there is no character literal](./107-a-small-char-is-one-object-and-there-is-no-char-literal.md)
- [ADR-108: The builder already holds the preheader, so the pass is not needed](./108-the-builder-holds-the-preheader-so-the-pass-is-not-needed.md)
- [ADR-109: Pages stay segregated by size class, and the header shrinks instead](./109-pages-stay-segregated-by-size-class.md)
- [ADR-110: A `Pure` boxing wrapper is a load, not a call](./110-a-pure-wrapper-is-a-load-not-a-call.md)
- [ADR-111: A `Text` literal's bytes are the compiler's promise, and the input's are the host's](./111-a-text-literals-bytes-are-the-compilers-promise.md)
- [ADR-112: The pacer has a ceiling, and only the live set may exceed it](./112-the-pacer-has-a-ceiling-and-the-live-set-may-exceed-it.md)
- [ADR-113: An `Int` box is a table read behind a pacing branch, and the token is permission to collect](./113-an-int-box-is-a-table-read-behind-a-pacing-branch.md)
- [ADR-114: The native roots are one store, and it grows because only their depth is bounded](./114-the-native-roots-are-one-store-and-only-their-depth-is-bounded.md)
- [ADR-115: A `Text` counts itself once, and the count is the licence to index its bytes](./115-a-text-counts-itself-once-and-the-count-is-the-licence.md)
- [ADR-116: A descriptor's address is the runtime's, and the compiler names a slot](./116-a-descriptors-address-is-the-runtimes-and-the-compiler-names-a-slot.md)
- [ADR-117: A raise that branches is its own observation, and only checked `Int` arithmetic can be one](./117-a-raise-that-branches-is-its-own-observation.md)
- [ADR-118: A `Vec[T]`'s three words are the compiler's to read, and a `VecDeque`'s are not (part 1 of 2 — W4a; W4b appends)](./118-a-vecs-three-words-are-the-compilers-to-read.md)
- [ADR-119: *owed* — W10, the inline scalar claim](./119-slug.md)
- [ADR-120: A box with one reader in its own block is not a box, and the value it cost the debugger is given back by a slot that knows it is not a reference (complete — part 1 W8-S0, part 2 W8-S0b)](./120-a-box-with-one-reader-in-its-own-block-is-not-a-box.md)
- [ADR-121: *reserved* — W8-S1, `Gc`→`Scalar` demotion for loop-carried locals; behind the wave-5 gate](./121-slug.md)
- [ADR-122: A descriptor the compiler wrote is provable, and a parameter is not](./122-a-descriptor-the-compiler-wrote-is-provable-and-a-parameter-is-not.md)
- [ADR-123: *reserved* — W12, two code variants selected by `--debug never`; handover 26 defers it](./123-slug.md)
