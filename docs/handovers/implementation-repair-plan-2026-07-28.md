# Implementation repair plan — 2026-07-28

Repair plan for
`docs/handovers/implementation-adversarial-audit-2026-07-28.md`. Every finding
in that audit was independently re-verified against the source before this plan
was written; the verification result is in §1. This document is the execution
order, not a second audit.

## 0. How to read this

The audit lists defects. This plan lists **work**. The two are not one-to-one:
139 findings collapse into **21 shared representation changes** (`F1`-`F21`,
§3) consumed by **21 stages** (`S1`-`S21`, §5). Most stages are cheap once
their foundation lands; the cost is concentrated in the foundations.

Three things gate execution and should be settled before any code is written:
the **hazards** in §6 (fixes that make the codebase *less* safe if landed out
of order), the **design decisions** in §7 (thirteen questions only the repo
owner can answer, three of which block a stage outright), and the **test
discipline** in §8 (five currently-*passing* tests assert the bugs and must be
rewritten, and one ignored test hangs the process if run before its fix).

## 1. Verification result

All 139 findings were re-derived from the code by thirteen independent readers,
one per subsystem, each instructed to refute the audit where it overstated.
**None were refuted.**

| Result | Count |
|---|---|
| CONFIRMED | 135 |
| PARTIAL | 4 |
| REFUTED | 0 |
| ALREADY-FIXED | 0 |
| UNCERTAIN | 0 |

The four `PARTIAL` findings (FE-08, IP-11, MIR-10, TY-24) each retain a real
defect under a narrowed claim: **TY-24** — the duplicate-declaration overwrite
is real, but the actual defect is `ScopeTree::bind`'s doc/signature mismatch;
**MIR-10** — the verifier is genuinely absent, but its scalar-across-safepoint
rule initially fails on the eager `lower_seq_*` lowerers, so it must land
behind a flag; **IP-11** — the missing atomics are real, but `uint` has no
runtime descriptor and must not ship as specified; **FE-08** — the fuzz
generators are indeed UTF-8-only, but this is test coverage rather than a
product defect.

### 1.1 Severity reassessments

Seven severities moved. Six moved **down** — real, but not the release blockers
the audit claims — and one moved **up**. This changes what must be fixed before
the next release versus what can be scheduled.

| ID | Audit | Verified | Why |
|---|---|---|---|
| P0-09 | P0 | **P2** | Latent, not live: no payload type currently requests an alignment greater than the header's. Still fix it — as layout hygiene inside S4, not as a shipping blocker. |
| P0-10 | P0 | **P1** | Requires two heaps in one process, which only the debugger creates, and ADR-032 already routes debugger allocation to the main heap. |
| P0-13 | P0 | **P1** | The mismatch is real but benign on the supported host; a portability and maintenance defect, not live UB. |
| P0-14 | P0 | **P2** | Reachable only from a corrupt `RawSyntaxKind`, which nothing constructs from untrusted input today. |
| RT-05 | P0 | **P1** | `Runtime::heap_mut().reset()` is reachable but has no in-tree caller that survives the reset. Delete the accessor rather than treat it as a live blocker. |
| DBG-01 | P0 | **P1** | Confined to the debugger's REPL evaluator; it cannot corrupt a normal run. |
| DBG-04 | P0 | **P1** | Same containment. Generated prologues mask the argument case; only omitted snapshot locals are exposed. |
| IPR-03 | P1 | **P0** | **Upgraded.** `sections` hands the child a subslice at offset 0 while the resulting `Text` keeps the *original* buffer as owner, so later-section text reads bytes from the start of the input. A live wrong-answer-and-out-of-range-read path on ordinary AoC input, not a latent one. |

Two findings were split out of P0-08, which the audit bundled: **P0-08b** (22
boxed-return runtime wrappers omit `maybe_collect`) and **P0-08c** (the stdlib
`allocates` metadata disagrees with actual allocation behaviour). They have
different prerequisites and must not land together — see H2.

### 1.2 What the audit understated

**P0-02 cannot close its own invariant in the audit's stage 1.** Replacing
`Type(0)` with `Known | Opaque` is correct, but roughly 35 of the ~40 sites
have *no correct type available* until HIR-01 carries inferred per-use types
into lowering — which the audit schedules three stages later. The
representation lands early (S3) with those sites explicitly `Opaque`; the
verifier rule forbidding `Opaque` in a descriptor-producing position stays
**off** until S15. Turning it on with P0-02 would refuse to compile
currently-working programs.

**A declared dependency cycle** exists between P0-02 and P0-11 (each lists the
other as a prerequisite). It resolves cleanly: `descriptor_for_type` takes
`praxis_types::Type`, not `MirType`, so P0-02 lands first with `Opaque => emit
no descriptor` while the `Known` path keeps its `_ => INT` fallback; P0-11 then
makes the `Known` path exhaustive. Stated here because a planner that takes
both `depends_on` lists literally will deadlock.

**P0-04 alone makes the codebase quieter, not safer.** The arithmetic wrappers
already return the Unit sentinel on fault (`abi.rs:566`), and codegen calls
`Symbol::IntLoad` on that result at `lower.rs:877` *before* `call_check_fault`
at `:880` — an 8-byte read past a size-0 Unit payload, live today and
independent of P0-04. Worse, once fault epilogues return a real Unit instead of
`iconst 0`, a caller that `int_load`s a faulted return gets a silently wrong
value where it previously got a loud segfault. P0-04 and P0-08 must ship
together.

## 2. Shape of the work

The audit's 21 stages form five mostly-independent tracks over a small shared
prefix. The runtime spine is the critical path; the type spine is the longest.
Four further stages — **S23–S26**, for the defects §4.1 records as found *during*
the repair — hang off the end and depend on none of the tracks.

```text
  S1 runtime identity ─┬─> S4 ──> S5 ──> S6 ──> S7 ─┬─> S8   generation arenas
   (hard barrier)      │   layout  roots  pacing     ├─> S9   MIR root exactness
                       │                             └─> S10  compare / nominal
  S3 ABI + MirType ────┤                                        │
                       └─> S21  pipelines                        │
                                                                 │
  S11 ──> S13 ──> S14 ──> S15 ─┬─> S16  records / patterns        │
  TypeDb  decls   control-flow  └─> S17  capabilities ──> S18 <───┘  Option
                                        ^
  S12 frontend grammar ─────────────────┘ (also gates S16)

  S19 parser compile ──> S20 parser runtime   (S20's last item needs S5)

  S2  independent hardening — runs alongside everything

  S23 independent hardening, round two — likewise (§4.1)
  S24 function values (P0; needs D15) ── independent of every stage above
  S25 grammar completion ──> §3.3's program compiles
  S26 declaration / pattern / inference gaps
```

| Stage | Name | Findings | P0 | P1 | Weight |
|---|---|---|---|---|---|
| S1 | Runtime identity registry | 1 | 1 | 0 | 3 |
| S2 | Independent hardening | 13 | 1 | 4 | 19 |
| S3 | ABI manifest, MIR representation, fault-path values | 5 | 4 | 1 | 40 |
| S4 | Object layout and heap provenance | 2 | 0 | 1 | 4 |
| S5 | Root-set completeness and native RAII roots | 3 | 2 | 1 | 36 |
| S6 | Allocation pacing, effect metadata, heap lifecycle | 7 | 0 | 6 | 27 |
| S7 | Descriptor totality, typed collection construction, fault representation | 8 | 3 | 3 | 23 |
| S8 | Generation arena for JIT and plan metadata | 5 | 0 | 5 | 42 |
| S9 | MIR root exactness, debug/root split, verifier | 5 | 2 | 2 | 35 |
| S10 | Semantic comparison, nominal schema identity, debugger type recovery | 5 | 1 | 3 | 23 |
| S11 | TypeDb core: levels, schemes, nominal identity, validated constructors | 8 | 0 | 7 | 54 |
| S12 | Parser grammar: wildcard token, statement separators, struct-literal suppression | 4 | 0 | 3 | 13 |
| S13 | Annotations honored, declaration passes, mutability and scope discipline | 10 | 0 | 9 | 26 |
| S14 | Control flow: bottom type, contexts, joins, loop values | 6 | 0 | 4 | 26 |
| S15 | Per-use types into HIR and MIR, then monomorphization | 7 | 0 | 5 | 37 |
| S16 | Records, patterns, exhaustiveness, enum constructors | 5 | 0 | 5 | 25 |
| S17 | Constraint channel and capabilities | 11 | 0 | 8 | 66 |
| S18 | Option contract and enum nominal identity | 3 | 0 | 3 | 19 |
| S19 | Input-parser compile pipeline | 11 | 0 | 6 | 19 |
| S20 | Parser runtime cursor and region ownership | 14 | 1 | 11 | 51 |
| S21 | Pipeline plan representation and per-stage indices | 6 | 0 | 6 | 28 |
| | **Audit total** | **139** | **15** | **93** | **616** |
| S23 | Independent hardening, round two | 4 | 0 | 0 | 4 |
| S24 | Function values | 1 | 1 | 0 | 3 |
| S25 | Grammar completion | 4 | 0 | 2 | 17 |
| S26 | Declaration, pattern and inference gaps | 5 | 0 | 3 | 11 |
| | **With §4.1** | **153** | **16** | **98** | **651** |

Weight is S=1, M=3, L=8, XL=20 — relative sizing only, not days. The 21
foundations carry a further 208 weight, and most of the stage weight above is
discharged by them: per-finding work after its foundation lands is usually
small. Read the totals as "the foundations are the project; the findings are
the acceptance criteria".

**S23–S26 are not the audit's.** They are §4.1: fourteen defects found while
executing this plan, registered after S17 closed. They add 35 weight — about
5% — which is the honest measure of how much the audit missed, and the one P0
among them (REP-01, a host abort from a program that passes `praxis check`) is
the measure of how much that matters.

**Suggested release gates.** S1-S5 close every live memory-safety and
ABI-soundness defect and are the minimum before shipping anything. S6-S10
remove the descriptor and value lies. S11-S18 restore type soundness. S19-S21
fix wrong answers in the parser and pipelines. Only the first group is a hard
release blocker; the rest are correctness debt that can ship incrementally as
long as the ordering in §6 holds.

## 3. Foundations

These are the shared representation changes. A foundation qualifies only if two
or more findings depend on it, or if it makes a whole class of defect
structurally impossible. They are the direct application of the `AGENTS.md`
maxim: each deletes an illegal state rather than adding a check that catches
it. Tier is dependency depth — T0 has no prerequisites.

| ID | Foundation | Effort | Tier | Unblocks |
|---|---|---|---|---|
| F1 | praxis_runtime::descriptor: derived built-in type identity (BuiltinTypeId + sealed TypeId + `static` descriptors) | M | T0 | P0-01, P0-11, P0-12, RT-09, RT-10, RT-11, RT-12, RT-13, RT-16, DBG-01, DBG-02, IPR-13, IP-11, TY-25, MIR-16 |
| F2 | praxis_source::diagnostic::DiagCode: exhaustive diagnostic-code registry | S | T0 | TY-11, TY-12, TY-14, TY-15, TY-20, TY-23, TY-24, TY-28, TY-29, HIR-04, HIR-07, IP-04, IP-07, IP-09, IP-10, FE-04, MIR-09, RT-14 |
| F3 | praxis_syntax::ident: one Unicode identifier class + validated `Ident` newtype | M | T0 | FE-01, FE-02, HIR-05, IP-04, DBG-03, IP-11 |
| F4 | praxis_stdlib::abi::RuntimeSymbol: the single ABI signature + allocation-effect manifest | L | T0 | P0-08, P0-08c, P0-13, MIR-09, MIR-14, MIR-15, TY-33, RT-14, RT-15, P0-12, P0-04 |
| F5 | praxis_types: sealed `Type` handle + fallible TypeDb constructors + `TypeCtorError` | L | T0 | P0-02, TY-05, TY-06, TY-07, TY-12, HIR-04, MIR-16, TY-04 |
| F6 | praxis_runtime::gc::GcHeader repack: one layout authority + heap provenance | M | T0 | P0-09, P0-10, RT-01, RT-05, MIR-01 |
| F7 | praxis_runtime: sealed composite root set + `NativeScope`/`Rooted` + `Safepoint` token | XL | T0 | P0-06, P0-07, P0-08b, P0-10, RT-02, RT-05, DBG-04, IPR-14 |
| F8 | praxis_syntax::Token newline trivia + `StmtSeparator` | M | T0 | FE-04, FE-06, FE-05 |
| F9 | praxis_types::fold: one exhaustive `TypeFolder` with a cycle memo and identity preservation | M | T1 | TY-01, TY-02, TY-03, TY-04, TY-06, MONO-01, MONO-03, RT-12 |
| F10 | praxis_types: scheme-owned binders + constraint channel + `Level` newtype | XL | T1 | TY-01, TY-03, TY-22, TY-29, TY-30, TY-31, TY-32, RT-08, P0-12, MONO-01, MONO-02 |
| F11 | praxis-repr (NEW crate): the total, bidirectional `Type ⇄ TypeDescriptor` bridge | L | T1 | P0-11, P0-02, DBG-01, DBG-02, RT-10, RT-11, IPR-13, TY-32, RT-08, MIR-16 |
| F12 | praxis_types: nominal identity (`DefId + args`) + `TypeKey` + runtime `SchemaIdentity` | XL | T1 | TY-06, MONO-03, MONO-01, RT-12, RT-13, RT-14, RT-15, MIR-12, DBG-06, TY-04, TY-12 |
| F13 | praxis_codegen_cranelift::generation::Generation: one reclaimable JIT-generation arena | XL | T1 | MIR-12, MIR-13, DBG-05, DBG-06, IP-12, RT-12, RT-13, IP-07 |
| F14 | praxis_runtime::parser: absolute `Cursor`/`ByteRegion`/`Input` + validated `TextSlice` | XL | T1 | IPR-01, IPR-02, IPR-03, IPR-04, IPR-05, IPR-06, IPR-07, IPR-08, IPR-09, IPR-10, IPR-11, IPR-12, RT-06, IP-10 |
| F15 | praxis_hir: per-node inferred-type map (`NodeKey` → `Type`) + `MethodRef` | XL | T2 | HIR-01, HIR-02, HIR-09, MONO-01, MONO-02, P0-02, MIR-16, DBG-02, TY-30 |
| F16 | praxis_mir::ir::MirType + raw-word elimination (`LoadCapture`, `TupleShape`) | XL | T2 | P0-02, P0-03, MIR-05, MIR-16, P0-11, DBG-01, DBG-02, MIR-10 |
| F17 | praxis_mir: `RootSlots`/`DebugSlots` newtypes + effect-driven safepoints + `verify` pass | L | T2 | MIR-01, MIR-02, MIR-10, MIR-11, MIR-16, P0-03, P0-08, P0-02 |
| F18 | praxis_runtime::debug: `Option<GcRef>` debug values + optional static type | M | T2 | P0-04, MIR-09, MIR-16, MIR-01, DBG-02, P0-02 |
| F19 | praxis_hir::decl: sealed `TypeEnv` + `DeclGroup` two-phase inference driver | L | T2 | TY-01, TY-09, TY-10, TY-11, TY-13, TY-14, TY-15, TY-22, TY-23, TY-24, HIR-03 |
| F20 | praxis_hir::TypedExpr: one derived child walker | S | T2 | HIR-08, MONO-01, MONO-02, TY-17, TY-21, HIR-09 |
| F21 | `Option[T]` as a real prelude type: `TypePattern::Option` + prelude `EnumSchema` + honest `PreludeBinding` | L | T2 | RT-14, RT-15, TY-33, TY-34, RT-13, TY-06, IP-06 |

### 3.1 Foundation detail

#### F1 — praxis_runtime::descriptor: derived built-in type identity (BuiltinTypeId + sealed TypeId + `static` descriptors)

*Effort M · crates: `praxis-runtime`, `praxis-debugger`, `praxis-codegen-cranelift` · unblocks P0-01, P0-11, P0-12, RT-09, RT-10, RT-11, RT-12, RT-13, RT-16, DBG-01, DBG-02, IPR-13, IP-11, TY-25, MIR-16*

```rust
// crates/praxis-runtime/src/descriptor.rs
#[derive(Clone,Copy,PartialEq,Eq,PartialOrd,Ord,Hash,Debug)]
#[repr(u32)]
pub enum BuiltinTypeId {
    Unit=0, Bool, Int, Byte, Char, Float, Text,
    Vec, Deque, Grid, Map, Set, Counter, MinHeap, MaxHeap, BitSet,
    Tuple, Record, Enum, Closure, VarCell,
}
impl BuiltinTypeId {
    pub const COUNT: usize = 21;
    pub const fn from_u32(v: u32) -> Option<BuiltinTypeId>; // total match, no transmute
}

#[derive(Clone,Copy,PartialEq,Eq,PartialOrd,Ord,Hash,Debug)]
pub struct TypeId(u32);                      // WAS `pub struct TypeId(pub u32)` (descriptor.rs:17)
impl TypeId {
    pub const fn to_u32(self) -> u32 { self.0 }
    pub const fn as_builtin(self) -> Option<BuiltinTypeId> { BuiltinTypeId::from_u32(self.0) }
    #[cfg(any(test, feature="test-descriptors"))]
    pub const fn for_test(n: u32) -> TypeId { TypeId(u32::MAX - n) }   // outside 0..21 by construction
}

pub type CompareFn = unsafe fn(a:*const u8, b:*const u8) -> core::cmp::Ordering; // NEW
pub struct TypeDescriptor {
    id: TypeId,                              // PRIVATE — no integer literal may be written
    pub name: &'static str, pub size: usize, pub align: usize,
    pub trace: TraceFn, pub drop_value: DropFn, pub format: FormatFn,
    pub equals: Option<EqualsFn>, pub hash: Option<HashFn>,
    pub compare: Option<CompareFn>,          // NEW: ordering becomes a descriptor operation
}
impl TypeDescriptor {
    /// The ONLY constructor for a built-in. `id` is DERIVED from `b`.
    pub const fn builtin(b: BuiltinTypeId, name: &'static str, size: usize, align: usize,
                         trace: TraceFn, drop_value: DropFn, format: FormatFn,
                         equals: Option<EqualsFn>, hash: Option<HashFn>,
                         compare: Option<CompareFn>) -> TypeDescriptor;
    pub const fn id(&self) -> TypeId;
    pub const fn as_builtin(&self) -> Option<BuiltinTypeId> { self.id.as_builtin() }
}
pub const BUILTINS: [&TypeDescriptor; BuiltinTypeId::COUNT] = [ &scalars::UNIT, /* … */ ];
```
and each descriptor becomes `pub static UNIT: TypeDescriptor =
TypeDescriptor::builtin(BuiltinTypeId::Unit, "Unit", …);` — **`static`, not
`const`**.

TWO CRITIQUES OF THE AUDIT'S PROPOSAL:
(1) The audit keeps `id: TypeId::builtin(BuiltinTypeId::Float)` *written per descriptor* plus a `const _: () = assert!(unique)` over a `BUILTINS` array. That leaves the field writable (`Float`'s descriptor can still be labelled `Text`) and the const-assert cannot compile: descriptors must be `static` for pointer identity, and const-eval may not read a `static`'s value. Deriving `id` from the constructor argument and making the field private reduces uniqueness to enum-discriminant uniqueness, which rustc already enforces; the array check downgrades to an ordinary `#[test]`.
(2) `const X: &TypeDescriptor = &TypeDescriptor{…}` is a const-promoted rvalue with **no guaranteed unique address**. This is why `abi.rs:1256-1259` documents "compare by TypeId, not by pointer, because const descriptors may be duplicated across crate boundaries" — and that comment is correct *today*, contradicting P0-12/RT-11/RT-12/DBG-06 which all want pointer identity. Switching to `pub static` is the precondition that resolves the contradiction globally: after it, `core::ptr::eq(a,b)` is the authoritative identity test and `TypeId` is for diagnostics and the debugger's exhaustive match.

**Replaces**

21 independently-written `id: TypeId(N)` literals across
scalars.rs:57/89/121/153/194/242, text.rs:132, collections.rs:114/209/304,
maps.rs:140/220/324, heaps.rs:118/168, bitset.rs:127, records.rs:141,
enums.rs:104, tuples.rs:129, closures.rs:69, var_cell.rs:56 (Float and Text
both TypeId(5)); the 4 ad-hoc test descriptors at heap.rs:369/385 and
dynamic_key.rs:156/167; the debugger's hand-written integer match
`descriptor_id_to_type` (praxis-debugger/src/evaluate.rs:397-434); and the
pointer-vs-id identity contradiction at abi.rs:1256-1259.

**Order and risk**

ORDER: tier 0, no prerequisites. Land FIRST — every id-based guard proposed by
RT-09/RT-10/RT-11/P0-12 is a no-op for Float-vs-Text until it does, so shipping
those first creates false assurance. Blast radius: 49 `TypeId(` sites, 21
descriptor definitions, ~60 `scalars::INT`-style references become
`&scalars::INT` when the consts become statics. Adding `compare` touches all 21
+ 4 test descriptors mechanically. Judgement is confined to the debugger's
Unit/Byte/Float arms (which static `Type` each recovers — see F11) and to NaN
ordering semantics for `FLOAT.compare` (needs an ADR amending ADR-026).

#### F2 — praxis_source::diagnostic::DiagCode: exhaustive diagnostic-code registry

*Effort S · crates: `praxis-source`, `praxis-hir`, `praxis-input-parser`, `praxis-parser` · unblocks TY-11, TY-12, TY-14, TY-15, TY-20, TY-23, TY-24, TY-28, TY-29, HIR-04, HIR-07, IP-04, IP-07, IP-09, IP-10, FE-04, MIR-09, RT-14*

```rust
// crates/praxis-source/src/diagnostic.rs
#[derive(Clone,Copy,PartialEq,Eq,Hash,Debug)]
pub enum DiagCode {
    // Lex (T0xx) / Parse (P0xx) — existing, plus:
    ExpectedStatementSeparator,                                   // FE-04
    // Name (N0xx)
    UnknownName, UnknownType, NameIsNotAType, DuplicateDeclaration, NestedFunction,
    // Type (Y0xx)
    TypeMismatch, InfiniteType, NotEquatable, NotOrderable, NotHashable, NotNumeric,
    CollectionArity, AssignToImmutable, CompoundAssignNonNumeric,
    ReturnOutsideFunction, BreakOutsideLoop, IntLiteralOutOfRange,
    MissingRecordFields, UnknownRecordField, DuplicateRecordField,
    UnknownEnumVariant, NotAPatternForType, NonExhaustiveMatch, UnreachableArm,
    NoMethodOnType, InternalMissingType,
    // Input (I0xx)
    InvalidEscape, InvalidCaptureName, UnknownAtomic, UnknownConstructor,
    EmptySeparator, DuplicateSectionName, MisplacedRepeatedTail, ConstructorArity,
}
impl DiagCode {
    /// The ONE place a (category, number) pair is written. Exhaustive match.
    pub const fn code(self) -> DiagnosticCode;
    pub const fn ALL: &'static [DiagCode];
}
```
`DiagnosticCode::new` (diagnostic.rs:96) becomes `pub(crate)`; every
`err_diag`/constructor in praxis-hir/src/diagnostics.rs and praxis-input-parser
takes a `DiagCode`. A `#[test]` over `DiagCode::ALL` asserts `code()` is
injective.

WHY IT IS A FOUNDATION, NOT BOOKKEEPING: `DiagnosticCode::new(category,
number)` takes an open `u32`, so two unrelated diagnostics can silently share
`Y007`. Five audit groups independently propose new codes in the same Y0xx/N0xx
block (ty-scope wants 4, ty-flow wants 4, hir-mono wants 5, ip-compile wants 3,
fe wants 1) and each group's plan says "coordinate numbering with the other
groups before anyone allocates". With `DiagCode`, allocation is adding a
variant — which conflicts in git rather than colliding at runtime. Note
`DiagnosticCode::new(Type, 1234)` already exists in-tree as an unallocated
placeholder.

**Replaces**

Free-form `DiagnosticCode::new(DiagnosticCategory::Type, 7)` call sites across
crates/praxis-hir/src/diagnostics.rs, crates/praxis-input-parser (code_number
bridge), crates/praxis-parser; and the informal 'reserved block' convention in
the diagnostics.rs header.

**Order and risk**

ORDER: tier 0, no prerequisites. Land early and cheap so the ~20 new
diagnostics across all groups have one allocation authority. Mechanical. Only
real risk: insta snapshot churn if any existing code number moves — keep every
existing (category, number) pair byte-identical in `DiagCode::code()`.

#### F3 — praxis_syntax::ident: one Unicode identifier class + validated `Ident` newtype

*Effort M · crates: `praxis-syntax`, `praxis-parser`, `praxis-input-parser`, `praxis-debugger`, `praxis-hir` · unblocks FE-01, FE-02, HIR-05, IP-04, DBG-03, IP-11*

```rust
// crates/praxis-syntax/src/ident.rs   (NEW; praxis-syntax deps only praxis-source, so
// praxis-parser, praxis-input-parser, praxis-hir and praxis-debugger all reach it)
pub fn is_ident_start(c: char) -> bool;      // unicode_ident::is_xid_start(c) || c == '_'
pub fn is_ident_continue(c: char) -> bool;   // unicode_ident::is_xid_continue(c) || c == '_'

/// A validated identifier. `parse` is the only constructor, so a name that the
/// lexer would not have produced cannot exist downstream.
#[derive(Clone,PartialEq,Eq,Hash,Debug)]
pub struct Ident(Box<str>);
pub struct InvalidIdent { pub text: String, pub at: usize }
impl Ident {
    pub fn parse(s: &str) -> Result<Ident, InvalidIdent>;   // rejects "", "_", bad start/continue
    pub fn as_str(&self) -> &str;
}
impl std::ops::Deref for Ident { type Target = str; }
```
Lexer side (crates/praxis-parser/src/lex.rs): `Lexer` drops its `src: &'a [u8]`
field and keeps `&'a str`; `classify_byte(u8) -> ByteClass` (lex.rs:92) becomes
`classify(&self, at: usize) -> (CharClass, usize /*scalar len*/)` with an ASCII
fast path and a `chars().next()` decode for `>= 0x80`; `is_ident_continue(u8)`
(lex.rs:405-411, whose blanket `b >= 0x80` arm accepts every UTF-8 continuation
byte) is deleted; `diagnose_unknown` (lex.rs:373) advances by scalar length;
the `from_utf8(..).expect("ident slice is UTF-8")` at lex.rs:173 is deleted
because the slice is `&str` by construction.

SAME COMMIT, NON-OPTIONAL: `eat_ident` (lex.rs:174) must emit
`SyntaxKind::UNDERSCORE` when the finished run is exactly `"_"`. That single
line makes the already-written-but-dead wildcard path reachable — parse.rs:128
`is_pattern_start`, parse.rs:988 `parse_pattern`, nodes.rs:697 `Pattern::kind`,
resolve.rs:523, lower.rs:1899 `TypedPattern::Wildcard` — and is the *entirety*
of HIR-05's fix. Coupled decision the planner must make explicitly: `let _ =
f()`, `fn g(_)`, `|_|` currently go through `expect(Ident, …)` at
parse.rs:417/453/743 and would become parse errors; either accept `UNDERSCORE`
there with `TypedParam.name: ParamName::{Named(SymbolId), Wildcard}`, or
reject.

**Replaces**

crates/praxis-parser/src/lex.rs:99 (`b >= 0x80 => IdentStart` blanket) and :409
(`is_ident_continue(u8)` with its stale TODO);
crates/praxis-input-parser/src/scan.rs:194-195 (`split_capture`'s independent
ASCII-only rule, which silently degrades a malformed capture name to an
anonymous capture); crates/praxis-debugger/src/evaluate.rs:365-377
(`sanitize_name`'s `is_ascii_alphanumeric` rewrite, which is non-injective and
can collide two synthetic params).

**Order and risk**

ORDER: tier 0. Adds `unicode-ident` to `[workspace.dependencies]` (no_std, zero
transitive deps); std `char::is_alphabetic`+`'_'` is an acceptable fallback
since §4.1 does not mandate XID. Risk is concentrated in the `_`-as-binding
decision above, which changes what parses; grep the integration corpus
(`**/*.px`) — currently 0 hits — before landing. `DBG-03` must switch
`sanitize_name` to *reject* (`Result<Ident, InvalidIdent>`) rather than
rewrite; its test `sanitize_rejects_digit_leading_and_punct` (evaluate.rs:476)
is an ACTIVE test pinning the defective behavior and must be rewritten, not
extended.

#### F4 — praxis_stdlib::abi::RuntimeSymbol: the single ABI signature + allocation-effect manifest

*Effort L · crates: `praxis-stdlib`, `praxis-runtime`, `praxis-mir`, `praxis-codegen-cranelift`, `praxis-debugger` · unblocks P0-08, P0-08c, P0-13, MIR-09, MIR-14, MIR-15, TY-33, RT-14, RT-15, P0-12, P0-04*

```rust
// crates/praxis-stdlib/src/abi.rs  (praxis-stdlib deps only praxis-source → no cycle;
// praxis-runtime gains a praxis-stdlib dep, praxis-mir/-codegen already have one)
#[derive(Clone,Copy,PartialEq,Eq,Hash,Debug)]
pub enum RuntimeSymbol { AllocInt, AllocBool, AllocUnit, AllocText, AllocChar, AllocFloat,
    IntLoad, IntAdd, /* … ~90 variants, one per `praxis_*` extern … */ VecNew, VecPush, VecLen,
    StructEq, StructCmp /*new, P0-12*/, RaiseIntOverflow, RaiseDivByZero /*new, P0-08*/,
    RaiseEmptyCollection /*new, MIR-09*/, ClosureCapture, PushShadowFrame, /* … */ }

#[derive(Clone,Copy,PartialEq,Eq,Debug)] pub enum AbiKind { Ctx, Gc, RawI64, RawU32, Ptr }
#[derive(Clone,Copy,PartialEq,Eq,Debug)] pub enum AbiRet  { Gc, RawI64, Void }
/// The ONE answer to "does this call need a GC root set / a fault check?"
#[derive(Clone,Copy,PartialEq,Eq,Debug)]
pub enum Effect { Pure, Faults, Allocates, AllocatesAndFaults }
impl Effect { pub const fn allocates(self)->bool; pub const fn faults(self)->bool; }

pub struct AbiSig { pub params: &'static [AbiKind], pub ret: AbiRet, pub effect: Effect }
impl RuntimeSymbol {
    pub const ALL: &'static [RuntimeSymbol];
    pub const fn name(self) -> &'static str;        // exhaustive match
    pub const fn sig(self) -> &'static AbiSig;      // exhaustive match
    pub const fn effect(self) -> Effect { self.sig().effect }
}

// crates/praxis-runtime/src/abi.rs — the ONE pointer table
pub fn address(sym: RuntimeSymbol) -> *const u8;   // exhaustive match; new variant = compile error

// crates/praxis-stdlib/src/catalog.rs
pub enum MethodLowering { RuntimeSymbol(RuntimeSymbol), Intrinsic(Intrinsic) }
// MethodEntry.allocates: bool  (catalog.rs:62) is DELETED — the effect comes from the symbol.

// crates/praxis-mir/src/ir.rs
pub enum CallTarget { User(String), Runtime(RuntimeSymbol) }   // was Runtime(String)

// crates/praxis-codegen-cranelift/src/lower.rs
fn signature_for(sym: RuntimeSymbol, isa: &dyn TargetIsa) -> Signature;  // derived from sig()
```
Call convention comes from `module.isa().default_call_conv()` and pointer width
from `module.target_config().pointer_type()`, deleting the 16 literal
`CallConv::Fast` and `const GC: types::Type = I64` (lower.rs:28).

WHY ONE TABLE, NOT TWO: the audit proposes P0-13's signature registry and
P0-08c's allocation-effect table separately. They are the same table indexed by
the same symbol. Today "does this allocate?" has four independent answers that
disagree — `MethodEntry.allocates` (catalog.rs:62, read only by a test at
catalog.rs:280), the presence of a `live_roots` field on the `Inst` variant,
`liveness::safepoint_roots_slot`'s match (liveness.rs:253), and what the
codegen arm actually emits. `Inst::IntBinOp` has no `live_roots` field, is
absent from `safepoint_roots_slot`, and its codegen arm emits two
`praxis_alloc_int` calls (P0-08). With this manifest the MIR safepoint decision
is `callee.effect().allocates()` — one read, no drift — and the arity-only
`runtime_funcref` path can no longer feed an `I64` immediate into a `u32`
parameter (lower.rs:564, 1080).

**Replaces**

crates/praxis-codegen-cranelift/src/symbols.rs:15-160 (~90-arm name→pointer
string match); crates/praxis-codegen-cranelift/src/module.rs:55-114 (an
INDEPENDENT 57-name registration list that has already drifted — MIR-14);
lower.rs:1388-1424 (`runtime_funcref`'s arity-only signature synthesis);
lower.rs:1700-1802 (12 hand-written `*_sig()` builders);
lower.rs:174-186/445-452 (two duplicate definitions of the entry signature);
the 21 `CallTarget::Runtime("praxis_…".to_string())` string literals in
praxis-mir/src/build.rs; and `MethodEntry.allocates` on all 119 catalog
entries.

**Order and risk**

ORDER: tier 0 (no prerequisites), but MUST land BEFORE any finding that adds a
runtime symbol — P0-12 (`praxis_struct_cmp`), MIR-09
(`praxis_raise_empty_collection`), P0-08
(`praxis_raise_int_overflow`/`_div_by_zero`), TY-33 (`praxis_panic` et al.),
RT-14/RT-15 — or each one has to be added to five places again. Judgement
calls: the `AbiKind` vocabulary, and whether the debugger keeps its 7-arm
transmute ladder (evaluate.rs:298-353) or goes through a generated trampoline;
either way `null_sentinel()` (evaluate.rs:358-363, a `NonNull::dangling()`
feeding the phantom `RunnableFunction` parameter at module.rs:25) becomes dead
and must be deleted — it is the second instance of P0-04's invalid-GcRef
pattern. Also install `JITBuilder::symbol_lookup_fn` and stop relying on the
dlsym fallback, so an unregistered symbol is a `JitError` rather than a
platform-dependent success.

#### F5 — praxis_types: sealed `Type` handle + fallible TypeDb constructors + `TypeCtorError`

*Effort L · crates: `praxis-types`, `praxis-hir`, `praxis-input-parser`, `praxis-mir`, `praxis-codegen-cranelift` · unblocks P0-02, TY-05, TY-06, TY-07, TY-12, HIR-04, MIR-16, TY-04*

```rust
// crates/praxis-types/src/type_id.rs
pub struct Type(u32);          // WAS `pub struct Type(pub u32)` (type_id.rs:20)
impl Type { pub const fn to_u32(self) -> u32; }
//  NO public constructor. `pub(crate) const fn from_raw(u32)` is used only by db.rs.

// crates/praxis-types/src/db.rs
pub(crate) fn intern(&mut self, data: TypeData) -> Type;   // WAS pub (db.rs:128)

// crates/praxis-types/src/error.rs (NEW)
#[derive(Clone,PartialEq,Eq,Debug)]
pub enum TypeCtorError {
    TupleArity(usize),                                      // < 2
    CollectionArity { ctor: CollectionCtor, got: usize, want: usize },
    DuplicateField(String), DuplicateVariant(String),
}

// validated aggregate payloads — the only way to build a def
pub struct FieldSet(Box<[RecordFieldDef]>);
impl FieldSet { pub fn new(v: Vec<RecordFieldDef>) -> Result<FieldSet, TypeCtorError>; }
pub struct VariantSet(Box<[EnumVariantDef]>);
impl VariantSet { pub fn new(v: Vec<EnumVariantDef>) -> Result<VariantSet, TypeCtorError>; }

pub enum CollectionArgs { Nullary, Unary(Type), Binary(Type, Type) }   // shape == arity
pub enum TupleElems { /* private */ }
impl TupleElems { pub fn new(v: Vec<Type>) -> Result<TupleElems, TypeCtorError>; } // >= 2

impl TypeDb {
    pub fn tuple(&mut self, els: TupleElems) -> Type;
    pub fn collection(&mut self, ctor: CollectionCtor, args: CollectionArgs)
        -> Result<Type, TypeCtorError>;
    pub fn register_record(&mut self, name: Option<String>, fields: FieldSet) -> RecordDefId;
    pub fn register_enum(&mut self,   name: Option<String>, variants: VariantSet) -> EnumDefId;
}

// crates/praxis-types/src/data.rs — payload normalization (TY-05)
pub struct EnumVariantDef { pub name: String, pub payload: Vec<Type> }  // WAS Option<Vec<Type>>;
//   empty == payload-less. data.rs:177 already documents the two as equivalent.
```
Sealing `Type.0` is what makes P0-02 *durable*: today `Type(0)` is a legal
expression anywhere in the workspace, so MIR can (and does, 40 times in
build.rs) mint a forged arena handle. After sealing, the only producers are
`TypeDb::intern`/`fresh_var`, and MIR must say `MirType::Opaque` (F16) because
it cannot say `Type(0)`.

**Replaces**

`Type(0)` as an 'unknown' sentinel — 40 sites in
crates/praxis-mir/src/build.rs; `TypeDb::intern`'s public back door
(db.rs:128); unchecked `db.tuple(vec![x])` (lib.rs:83) and `db.collection(ctor,
args)` (lib.rs:90, which ignores `CollectionCtor::arity`, type_pattern.rs:85);
duplicate-field/variant acceptance in
`register_record`/`anon_record`/`register_enum`/`anon_enum`
(db.rs:270/289/314/335); the three-way `Option<Vec<Type>>` payload match at
unify.rs:275-298 and its `.clone().unwrap_or_default()` mirrors at
lower.rs:1591 and infer.rs:486; and infer.rs:414-418's manual payload
normalization.

**Order and risk**

ORDER: tier 0. Land BEFORE F16 (MirType) — otherwise the `Type(0)` sites can be
'fixed' one by one and reintroduced. ~40 non-test call sites (8 `db.tuple(`, 14
`.collection(`, 5 `.vec(`, 6 record ctors, 8 enum ctors) plus ~30 in
types_tests.rs; the TY-05 payload change alone is ~19 sites and entirely
compiler-found. The judgement is the fallible-constructor plumbing:
crates/praxis-input-parser/src/synthesize.rs returns a bare `Type` at
:53/:80/:94/:101/:172/:197 and has NO error channel — thread `Result` there
rather than `expect`, even though validate.rs:69/111/147/212 means it currently
runs on validated input. Also `Type.0` privacy breaks
praxis-hir/src/exhaustive.rs:231 and praxis-types/src/generalize.rs:121.

#### F6 — praxis_runtime::gc::GcHeader repack: one layout authority + heap provenance

*Effort M · crates: `praxis-runtime`, `praxis-codegen-cranelift` · unblocks P0-09, P0-10, RT-01, RT-05, MIR-01*

```rust
// crates/praxis-runtime/src/gc.rs
#[derive(Clone,Copy,PartialEq,Eq,Debug)]
pub struct HeapId(NonZeroU32);            // minted by a process AtomicU32 in Heap::new

#[repr(C)]
pub struct GcHeader {
    descriptor: *const TypeDescriptor,     //  0..8   (null == swept/poisoned)
    size: u32,                             //  8..12
    payload_offset: u16,                   // 12..14  THE single layout authority
    mark: Cell<u8>,                        // 14..15
    _pad: u8,                              // 15..16
    heap_id: u32,                          // 16..20  (0 == poisoned; else a live HeapId)
}                                          // 24 bytes, align 8
impl GcHeader {
    /// Compile-time authority used by the allocator AND by codegen.
    pub const fn payload_offset_for(payload_align: usize) -> usize;   // round_up(size_of::<Self>(), align)
    #[inline] pub fn payload<T>(&self) -> *mut T {
        unsafe { (self as *const _ as *mut u8).add(self.payload_offset as usize) as *mut T }
    }
    #[inline] pub fn heap_id(&self) -> Option<HeapId>;
    #[inline] pub fn is_poisoned(&self) -> bool;   // descriptor.is_null()
}
```
`Heap::alloc_raw` (heap.rs:172-216) writes `payload_offset` from the same
`round_up` it already computes at heap.rs:185 and stamps `heap_id`.
`Heap::mark` (heap.rs:252-285) rejects (debug-panics on) any root whose
`heap_id != self.id` **before the first `header.descriptor()` deref**, an O(1)
test that closes P0-10 without the XL `GcRef<'h>` branding the audit correctly
rejects. `Heap::sweep` (heap.rs:288-308) poisons the header (`descriptor =
null; heap_id = 0`) before `swap_remove`, so a stale `GcRef` is rejected by the
same check rather than traced through freed storage. `Heap::reset`
(heap.rs:312-324) mints a fresh `HeapId`, which also closes RT-05's
dangling-immortal window.

CALIBRATION (against the audit, which rates P0-09 P2 — correctly): the current
mismatch between `alloc_raw`'s `round_up(header_size, payload_align)`
(heap.rs:185) and `payload()`'s bare `size_of::<GcHeader>()` (gc.rs:65-70) is
**latent, not live** — they agree for every `payload_align <= 16`, and no
in-tree payload over-aligns. It is a foundation anyway because P0-10's
`heap_id` must go into the same `#[repr(C)]` struct and codegen hardcodes the
header size (lower.rs:1097) — repack ONCE.

**Replaces**

Three independent copies of the object layout:
crates/praxis-runtime/src/heap.rs:185/196 (`round_up(header_size,
payload_align)`), crates/praxis-runtime/src/gc.rs:65-70
(`add(size_of::<GcHeader>())`), and
crates/praxis-codegen-cranelift/src/lower.rs:1097 (`size_of::<GcHeader>()`
inlined into the enum-payload access). Also replaces `GcRef` having no
provenance at all (gc.rs:94-95), which is what lets
`Heap::collect`/`RootScope::root` accept a foreign or freed ref today.

**Order and risk**

ORDER: tier 0. Header grows 16→24 bytes and `#[repr(C)]` layout changes —
codegen reads it, so bump RUNTIME_ABI_VERSION (abi.rs:48) +
COMPILER_EXPECTED_ABI_VERSION (abi.rs:70) in the SAME batched bump as F7's
`native_roots` and F18's `DebugLocal` change, not three separate bumps. All 122
`payload::<T>()` uses across 15 files route through
`GcHeader::payload`/`GcRef::payload` and need no edit. Watch: the 4 hand-built
`GcHeader{…}` literals in roots.rs tests and heap.rs tests construct the struct
directly and must gain the new fields.

#### F7 — praxis_runtime: sealed composite root set + `NativeScope`/`Rooted` + `Safepoint` token

*Effort XL · crates: `praxis-runtime`, `praxis-debugger`, `praxis-codegen-cranelift`, `praxis-cli` · unblocks P0-06, P0-07, P0-08b, P0-10, RT-02, RT-05, DBG-04, IPR-14*

```rust
// crates/praxis-runtime/src/roots.rs
pub trait RootSet { fn push_roots(&self, out: &mut Vec<GcRef>); }     // unchanged

/// The ONLY thing `Heap::collect` accepts. Sealed: constructible only from a
/// `*mut RuntimeContext`, so "collect against a partial root set" is unrepresentable.
pub struct RuntimeRoots<'a> {
    shadow:        Option<&'a ShadowFrame>,     // ctx.roots — null is NO LONGER an early return
    input:         Option<GcRef>,               // ctx.input_source     (P0-06)
    parse_partial: Option<GcRef>,               // (*ctx.parse_detail).fail.partial
    snapshot:      Option<&'a CrashSnapshot>,   // (*ctx.crash_snapshot)
    native:        Option<&'a NativeRootFrame>, // ctx.native_roots     (NEW field)
}
impl<'a> RuntimeRoots<'a> {
    /// # Safety: `ctx` live and wired.
    pub unsafe fn from_context(ctx: *mut RuntimeContext) -> RuntimeRoots<'a>;
    /// Mint the pacing token. The only producer of `Safepoint`.
    pub fn pace<'s>(&'s self) -> Safepoint<'s>;
}
impl RootSet for RuntimeRoots<'_> { /* exhaustive over all five fields */ }

#[repr(C)] pub struct NativeRootFrame { parent: *mut NativeRootFrame, roots: Vec<GcRef> }
impl RootSet for NativeRootFrame { /* walks the parent chain */ }

/// RAII: pushes onto `ctx.native_roots` on `new`, pops on `Drop`.
pub struct NativeScope<'c> { ctx: *mut RuntimeContext, frame: Box<NativeRootFrame>,
                             _m: PhantomData<&'c mut RuntimeContext> }
impl<'c> NativeScope<'c> {
    pub unsafe fn new(ctx: *mut RuntimeContext) -> NativeScope<'c>;
    pub fn root(&mut self, r: GcRef) -> Rooted<'_>;
    pub fn vec(&mut self) -> ScopedVec<'_>;      // a Vec<GcRef> that IS a root
}
/// A `GcRef` proven rooted for `'s`. The ONLY input to a `&mut Payload` accessor.
#[derive(Clone,Copy)] pub struct Rooted<'s> { r: GcRef, _m: PhantomData<&'s ()> }

// crates/praxis-runtime/src/heap.rs — signature change is the point
#[must_use] pub struct Safepoint<'a>(PhantomData<&'a ()>);
impl Heap {
    pub fn collect(&self, roots: &RuntimeRoots<'_>);                 // was &dyn RootSet
    pub fn maybe_collect(&self, roots: &RuntimeRoots<'_>) -> bool;   // was &dyn RootSet
    pub fn alloc_with(&self, sp: Safepoint<'_>, …) -> GcRef;         // pacing unforgeable
    #[cfg(test)] pub fn collect_with(&self, roots: &dyn RootSet);    // test-only escape hatch
}

// crates/praxis-runtime/src/context.rs — appended AFTER crash_snapshot (context.rs:245)
pub struct RuntimeContext { …, pub native_roots: *mut NativeRootFrame }
```
`abi::maybe_collect` (abi.rs:101-114) becomes
`heap(ctx).maybe_collect(&RuntimeRoots::from_context(ctx))` with the
`roots_ptr.is_null()` early return at abi.rs:107 **deleted** — that early
return is what makes input/parse-detail/snapshot/native invisible to automatic
GC whenever no generated frame is on the stack.

THREE CORRECTIONS TO THE AUDIT:
(1) Its `RuntimeRoots` sketch has `input: GcRef` non-optional. `RuntimeContext::placeholder` (context.rs:256-271) sets `input_source` to a placeholder and `Runtime::context` may run before the buffer exists — it must be `Option<GcRef>` or the placeholder path traces garbage.
(2) P0-08b proposes moving `maybe_collect` *inside* `Heap::alloc`. It cannot: `Heap` (heap.rs:34) is ctx-agnostic and has no route to the root set. The `#[must_use] Safepoint` token above is the workable form of the same invariant.
(3) The real representational lie behind P0-07 is `unsafe fn *_payload_mut(r: GcRef) -> &'static mut T` (abi.rs:736-741, 2283-2289 and ~10 collection siblings). Changing those to take `Rooted<'s>` and return `&'s mut T` is what makes 'hold a `&mut Payload` across a safepoint without rooting the owner' fail to type-check.

**Replaces**

crates/praxis-runtime/src/abi.rs:101-114 (`maybe_collect` walking only
`ctx.roots`, with a null early return); the unreachable `impl RootSet for
CrashSnapshot` (crash_snapshot.rs:106-118) and the missing impls for
`ParseDetail`/`ParseFail.partial` (parse_detail.rs:52, :100-107) and
`SnapshotSlot` (crash_snapshot.rs:126-156); `RootScope`/`RootScope::child`
(roots.rs:35-84) as a peer of the shadow chain rather than an arm of it — which
is why crates/praxis-debugger/src/evaluate.rs:254-260's root scope is inert;
and the ~60 `*_payload_mut(GcRef) -> &'static mut T` accessors in abi.rs.

**Order and risk**

ORDER: tier 0 for `RuntimeRoots` (P0-06); `NativeScope`/`Rooted` (P0-07)
follows immediately. CRITICAL SEQUENCING: P0-08b (adding `maybe_collect` to the
22 unpaced wrappers) and IPR-14 (adding safepoints to the parser interpreter)
MUST NOT land before this — adding safepoints while the root set is incomplete
converts a memory-growth bug into a use-after-free. `Heap::collect` signature
change touches 15 call sites (runtime tests, crash_snapshot.rs:301,
text.rs:197, context.rs:831) + jit.rs:4235. `RuntimeContext` gains a field:
batch the ABI bump with F6 and F18. Test-infrastructure gap the audit
understates: P0-06's three sub-defects, P0-07, P0-08b and P0-08c have NO
`#[ignore = "known bug: …"]` gating test — a poisoned-arena or Miri lane plus
the existing `allocate_until_automatic_collection` helper is a prerequisite for
verifying the fix.

#### F8 — praxis_syntax::Token newline trivia + `StmtSeparator`

*Effort M · crates: `praxis-syntax`, `praxis-parser` · unblocks FE-04, FE-06, FE-05*

```rust
// crates/praxis-syntax (Token)
pub struct Token { pub kind: SyntaxKind, pub span: TextRange,
                   /// True iff the trivia run immediately before this token contained
                   /// a `\n`/`\r`, or ended a `LineComment`. Set in `Lexer::eat_whitespace`.
                   pub preceded_by_newline: bool }

// crates/praxis-parser/src/parse.rs
pub enum StmtSeparator { Semicolon(Token), Newline, EndOfBlock }
impl Parser {
    fn newline_before(&self) -> bool;                       // reads the pending token
    /// Returns the separator it consumed, or emits DiagCode::ExpectedStatementSeparator
    /// and returns None. Called at the END of BOTH statement loops.
    fn expect_stmt_separator(&mut self) -> Option<StmtSeparator>;
}
```
Returning `StmtSeparator` rather than `bool` is the point: the statement loop
cannot advance without either producing a separator value or emitting a
diagnostic, so 'two statements adjacent with no separator' has no accepted
representation.

**Replaces**

crates/praxis-parser/src/parse.rs:341-350 (`eat_trivia`, which discards the
newline fact entirely); parse.rs:177-196 (top-level loop with NO separator
handling — a `;` at top level is also not consumed); parse.rs:569-589 (block
loop where `self.eat(SEMICOLON)` at :583 is unconditional/optional); the
newline-blind `starts_expr` at parse.rs:234-249 used by `break`/`return`
(parse.rs:917/934) to decide whether a value follows; and parse.rs:955-977's
match-arm loop, whose comment at :968 claims arms are 'comma-OR-newline
separated' while termination actually relies on `is_pattern_start` at :972.

**Order and risk**

ORDER: tier 0, but FE-06's match-arm half depends on it — passing
`StructLit::Allowed` into match-arm bodies before arm separation is
newline-aware will mis-parse `match x { A => Point { x: 1 } B => … }`. `Token`
is `Copy`; adding a bool touches only 2 construction sites (lex.rs:89, :388)
plus test constructors. HIGHEST TEST CHURN of any foundation: ~40 insta
snapshots in parse.rs, plus every `crates/praxis-cli/tests/fixtures/run/*.px`
with same-line statements. Judgement: newline must terminate a statement
without terminating a mid-Pratt expression — `1 +\n2` must still parse, so the
check applies only at statement boundaries and at `break`/`return`'s
optional-value decision.

#### F9 — praxis_types::fold: one exhaustive `TypeFolder` with a cycle memo and identity preservation

*Effort M · crates: `praxis-types` · unblocks TY-01, TY-02, TY-03, TY-04, TY-06, MONO-01, MONO-03, RT-12*

```rust
// crates/praxis-types/src/fold.rs   (NEW)
pub trait TypeFolder {
    fn db(&mut self) -> &mut TypeDb;
    fn fold_scalar(&mut self, t: Type, s: ScalarType) -> Type { t }
    fn fold_unit(&mut self, t: Type) -> Type { t }
    fn fold_never(&mut self, t: Type) -> Type { t }               // once Never is its own variant
    fn fold_tuple(&mut self, t: Type, els: &[Type]) -> Type;
    fn fold_func(&mut self, t: Type, params: &[Type], result: Type) -> Type;
    fn fold_collection(&mut self, t: Type, ctor: CollectionCtor, args: &[Type]) -> Type;
    fn fold_record(&mut self, t: Type, def: RecordDefId, args: &[Type]) -> Type;
    fn fold_enum(&mut self, t: Type, def: EnumDefId, args: &[Type]) -> Type;
    fn fold_var(&mut self, t: Type, v: VarId, st: &VarState) -> Type;
}

/// The ONE walk over `TypeData`. Exhaustive match, NO `_` arm, plus a
/// `HashMap<Type,Type>` memo so record/enum side-table recursion terminates.
pub fn fold<F: TypeFolder>(f: &mut F, t: Type) -> Type;

/// Identity preservation: a composite is re-interned only when a child actually
/// changed, so `instantiate` of a scheme with no applicable binder returns the
/// SAME handle and stops growing the arena unboundedly (TY-02).
enum Folded { Unchanged, Rebuilt(Type) }
```
The five existing walks become five folders, each losing its `_ =>` arm:
`clamp_levels` (ex-`lower_levels`), `occurs`, `generalize_walk`,
`instantiate_walk`, `deep_resolve`.

WHY THIS IS A FOUNDATION AND NOT CLEANUP: ADR-025 already records that
`instantiate_walk` silently lost the `Collection` arm exactly this way.
`deep_resolve` (db.rs:212-232) is the *same live bug today* — its `_ => t`
skips `Record` and `Enum`, so the crash debugger's static-type capture
(ADR-035) returns an unresolved record — AND it has **no cycle guard at all**,
which becomes a hang the moment nominal recursion exists (F12 introduces
`Record { def, args }`, making that reachable). One fold with no catch-all
makes an omitted variant a compile error in all five walks.

**Replaces**

crates/praxis-types/src/unify.rs:71-114 (`lower_levels`, whose comparison is
also reversed — TY-01), unify.rs:117-142 (`occurs`), generalize.rs:63-110
(`generalize_walk`), generalize.rs:128-193 (`instantiate_walk`, whose catch-all
also re-interns unchanged composites), and
crates/praxis-types/src/db.rs:212-232 (`deep_resolve`: `_ => t`, no memo).

**Order and risk**

ORDER: tier 1 — after F5 (sealed `Type`/validated ctors), because the folders
must re-intern through the checked constructors. Land BEFORE F10 (Scheme
reshape) and F12 (nominal identity), both of which are expressed as folders.
Contained to praxis-types; public signatures of
`generalize`/`instantiate`/`deep_resolve` do not change, so the 17
`.instantiate(` and 13 `resolve_type` call sites need no edits. Expect
infer_tests/mono snapshot movement from TY-02's identity preservation —
programs that currently depend on spurious var freshness will infer
differently.

#### F10 — praxis_types: scheme-owned binders + constraint channel + `Level` newtype

*Effort XL · crates: `praxis-types`, `praxis-stdlib`, `praxis-hir` · unblocks TY-01, TY-03, TY-22, TY-29, TY-30, TY-31, TY-32, RT-08, P0-12, MONO-01, MONO-02*

```rust
// crates/praxis-types/src/data.rs
#[derive(Clone,Copy,PartialEq,Eq,PartialOrd,Ord,Debug)]
pub struct Level(u32);
impl Level {
    /// The ONLY mutator. Monotone-decreasing, so ADR-008's invariant
    /// `level(w) := min(level(w), level(v))` cannot be written backwards.
    pub fn clamp_to(&mut self, outer: Level) { self.0 = self.0.min(outer.0); }
}
pub enum VarState { Unbound { level: Level }, Linked { target: Type } }
//   `VarState::Generalized` (data.rs:118) is DELETED — the arena no longer records
//   quantification, so no global mutation can desynchronize a scheme.

// crates/praxis-stdlib/src/capability.rs  (payload-free vocabulary; stdlib deps only source)
#[derive(Clone,Copy,PartialEq,Eq,Debug)]
pub enum CapKind { Eq, Ord, Hash, HashStable, Numeric }

// crates/praxis-types/src/constraint.rs   (NEW — the Type-carrying capabilities)
pub enum Capability { Kind(CapKind), Iterable { item: Type },
                      HasMethod { name: String, params: Vec<Type>, result: Type } }
pub struct Constraint { pub var: VarId, pub cap: Capability, pub at: FileSpan }

// crates/praxis-types/src/generalize.rs
pub struct Scheme { binders: Vec<VarId>, constraints: Vec<Constraint>, body: Type }
impl Scheme {
    pub fn monotype(body: Type) -> Scheme;      // binders/constraints empty BY CONSTRUCTION
    pub fn binders(&self) -> &[VarId];
    pub fn constraints(&self) -> &[Constraint];
    pub fn body(&self) -> Type;
}
impl TypeDb {
    pub fn generalize(&mut self, body: Type) -> Scheme;              // MUTATES NOTHING now
    pub fn instantiate(&mut self, s: &Scheme) -> Type;               // re-emits constraints
    pub fn instantiate_with_mapping(&mut self, s: &Scheme) -> (Type, Vec<Type>);  // MONO-01
    pub fn substitute(&mut self, t: Type, subst: &HashMap<Type,Type>) -> Type;    // MONO-01
    pub fn take_dischargeable(&mut self) -> Vec<Constraint>;         // drained after each unify
}

// crates/praxis-hir/src/capability.rs — ONE exhaustive decision function
pub fn check(db: &TypeDb, t: Type, cap: &Capability) -> Result<(), Type>;  // Err = offending inner type
```
THE CAPABILITY TRUTH PROBLEM the audit only half-states: `supports_hash`
(capability.rs:67-69) is literally `supports_eq`, and `supports_ord`
(capability.rs:82-114) claims Text and collections are orderable while the
runtime has no Text comparison and no elementwise collection ordering. Both
have ZERO non-test callers. The durable rule is **capability == the descriptor
has the callback** (`equals`/`hash`/`compare`, F1). praxis-hir cannot see
praxis-runtime, so wire it as a *test* in praxis-repr (F11): for every
`BuiltinTypeId`, assert `capability::check(db, t, cap).is_ok() ==
descriptor_for_type(db,t)?.supports(cap)`. That is the only honest
single-source-of-truth given the crate graph.

**Replaces**

`Scheme { pub quantified: Vec<VarId>, pub body: Type }` (generalize.rs:20-28)
and the global `TypeDb::generalize_var` (db.rs:247-250) that flips an arena
flag a *different* scheme may already depend on; `VarState::Generalized` and
its 4 readers (generalize.rs:131, data.rs:118, pretty.rs:116,
types_tests.rs:301 — the last a PASSING test asserting the arena flag); the
bare `level: u32` compared by hand at unify.rs:76; and the four dead/lying free
predicates in crates/praxis-hir/src/capability.rs:33/67/82/130 (only
`supports_eq` at infer.rs:1111 and `iter_item` at infer.rs:1284 have callers).

**Order and risk**

ORDER: tier 1 — after F9 (fold) and F5 (sealed Type), before F19 (declaration
pass) and MONO-*. `Scheme{..}` is literally constructed in ONE place
(generalize.rs:60) plus `Scheme::monotype` (generalize.rs:33), so the struct
reshape is nearly free; the surface is the 43
`generalize`/`instantiate`/`monotype` call sites (infer.rs 16, lower.rs 5,
mono.rs 2, types_tests.rs 20). praxis-lsp/-debugger/-cli have ZERO `scheme`
references. Judgement throughout: discharge ordering, level interaction with
generalization, and constraint origin spans (a constraint without `at` points
nowhere). NOTE FOR THE PLANNER: docs/decisions/010's final consequence defers
the §5.4 capability system to M7; M8 pipelines, M9 Option and ADR-037 floats
have all landed and ADR-026 assumes `supports_hash` is enforced. This is a
lapsed deferral coming due — supersede ADR-010's consequence when the channel
lands.

#### F11 — praxis-repr (NEW crate): the total, bidirectional `Type ⇄ TypeDescriptor` bridge

*Effort L · crates: `praxis-repr`, `praxis-runtime`, `praxis-codegen-cranelift`, `praxis-debugger`, `praxis-mir` · unblocks P0-11, P0-02, DBG-01, DBG-02, RT-10, RT-11, IPR-13, TY-32, RT-08, MIR-16*

```rust
// crates/praxis-repr/  (NEW leaf-ish crate; deps: praxis-types, praxis-runtime, praxis-stdlib)
//   praxis-codegen-cranelift and praxis-debugger both already dep types+runtime, so this
//   crate slots in with no cycle. (Zero-new-crate fallback: `praxis_mir::repr`, since
//   codegen and debugger both dep praxis-mir — but that drags in praxis-hir.)

pub struct NoRuntimeRepr { pub ty: praxis_types::Type, pub reason: &'static str }

/// Forward map. EXHAUSTIVE — no `_` arm anywhere. `Err` for Range/Seq/Func-
/// without-closure and for `TypeData::Var`: those are upstream compiler bugs and
/// the JIT must refuse to emit rather than mislabel.
pub fn descriptor_for_type(db: &TypeDb, ty: Type)
    -> Result<&'static TypeDescriptor, NoRuntimeRepr>;

/// Inverse map. Reads the *payload's* per-instance descriptors, so `Vec[Text]`
/// recovers as `Vec[Text]` and `Map[Text,Vec[Int]]` recovers faithfully.
/// # Safety: `value` must be a live GcRef.
pub unsafe fn type_for_value(value: GcRef, db: &mut TypeDb) -> Result<Type, NoRuntimeRepr>;

/// Capability agreement (see F10): asserted by a crate test over every BuiltinTypeId.
pub fn descriptor_supports(d: &TypeDescriptor, cap: CapKind) -> bool;

// crates/praxis-runtime — safe accessors so the bridge never does raw payload reads
pub fn collections::element_descriptor(r: GcRef) -> Option<&'static TypeDescriptor>;
pub fn maps::key_value_descriptors(r: GcRef) -> Option<(&'static TypeDescriptor, &'static TypeDescriptor)>;
pub fn tuples::schema_descriptors(r: GcRef) -> Option<&'static [*const TypeDescriptor]>;
```
Crate test enforcing the round-trip: for every `BuiltinTypeId` and every
constructed sample value, `descriptor_for_type(db, type_for_value(v)?)? ==
v.descriptor()` by **pointer** (valid once F1 makes descriptors `static`).

WHY THE INVERSE BELONGS IN THE SAME MODULE: today the two maps are written
independently and are not inverses — codegen sends `Float`→INT, `Unit`→INT,
`Record`→INT, `Enum`→INT (lower.rs:1649/1674/1676) while the debugger
reconstructs `Vec[?]`→`Vec[Int]` and `Map`→`Map[Int,Int]`
(evaluate.rs:406-429). ADR-035 decision point 4 asserts a soundness property
that is therefore false and must be amended as part of this work. Colocating
them makes the round-trip a compile-adjacent test instead of a hope.

**Replaces**

crates/praxis-codegen-cranelift/src/lower.rs:1611-1678 `descriptor_for_type`
(three `_ => INT` arms at :1649, :1674, :1676); lower.rs:1691
`collection_element_descriptor_for` (a pure alias);
crates/praxis-debugger/src/evaluate.rs:391-393 `descriptor_to_type` and
:397-434 `descriptor_id_to_type` (hand-written integer match with stale scalar
arms, `None` for Tuple/Record, and Int-defaulted element/key/value types).

**Order and risk**

ORDER: tier 1 — REQUIRES F1 (unique ids + `static` descriptors) and F5, and
must land WITH or BEFORE F16 (`MirType`), because `Result<_, NoRuntimeRepr>` is
what forces MIR to hold `Known(Type)` at the boundary. ONE judgement point:
what the JIT does on `Err` — emit a compiler diagnostic (correct) vs fall back
(reintroduces the bug). Expect PASSING tests to flip:
`vec_float_push_adopts_float_descriptor…` (adversarial_audit.rs:284) asserts
the current adopt-on-first-push retagging (abi.rs:1256-1263) which
`praxis_vec_push` must stop doing; those need rewriting, not unignoring.
DBG-02's truthfulness is bounded by P0-11: the stored `element_descriptor` is
itself a lie today for Grid positions/neighbors/find_all, Grid
cells/row/column, `chars(int)`, and single-anonymous templates.

#### F12 — praxis_types: nominal identity (`DefId + args`) + `TypeKey` + runtime `SchemaIdentity`

*Effort XL · crates: `praxis-types`, `praxis-runtime`, `praxis-codegen-cranelift`, `praxis-hir`, `praxis-input-parser`, `praxis-debugger` · unblocks TY-06, MONO-03, MONO-01, RT-12, RT-13, RT-14, RT-15, MIR-12, DBG-06, TY-04, TY-12*

```rust
// crates/praxis-types/src/data.rs
pub enum TypeData { …,
    Record { def: RecordDefId, args: Vec<Type> },      // WAS Record { def }
    Enum   { def: EnumDefId,   args: Vec<Type> },      // WAS Enum { def }
}
pub struct RecordDef { pub name: Option<String>, pub params: Vec<VarId>, pub fields: FieldSet }
pub struct EnumDef   { pub name: Option<String>, pub params: Vec<VarId>, pub variants: VariantSet }
//   `name: None` == anonymous/structural. A def is registered ONCE per declaration
//   (or once per prelude scheme), never per instantiation.
impl TypeDb {
    pub fn enum_type(&mut self, def: EnumDefId, args: Vec<Type>) -> Result<Type, TypeCtorError>;
    //   enforces args.len() == enum_def(def).params.len()
    pub fn option_def(&self) -> EnumDefId;              // ONE canonical prelude Option
    pub fn option_of(&mut self, t: Type) -> Type;
}

// crates/praxis-types/src/key.rs  (NEW)
#[derive(Clone,PartialEq,Eq,Hash,Debug)]
pub enum TypeKey { Scalar(ScalarType), Unit, Never, Tuple(Vec<TypeKey>),
    Func { params: Vec<TypeKey>, result: Box<TypeKey> },
    Collection { ctor: CollectionCtor, args: Vec<TypeKey> },
    Record(RecordDefId, Vec<TypeKey>), Enum(EnumDefId, Vec<TypeKey>), Var(VarId) }
impl TypeDb { pub fn canonical_key(&self, t: Type) -> TypeKey; }   // a `fold`

// crates/praxis-runtime/src/{records,enums,tuples}.rs — the runtime half
#[derive(Clone,Copy,PartialEq,Eq)]
pub enum SchemaIdentity { Nominal(u64 /* generation-scoped def key */), Anonymous }
#[repr(C)] pub struct RecordSchema { pub identity: SchemaIdentity, pub fields: &'static [RecordField] }
#[repr(C)] pub struct EnumVariantShape { pub name: &'static str, pub payload: &'static [*const TypeDescriptor] }
#[repr(C)] pub struct EnumSchema { pub identity: SchemaIdentity, pub name: &'static str,
                                  pub variants: &'static [EnumVariantShape] }
#[repr(C)] pub struct EnumPayload { pub schema: *const EnumSchema, pub tag: u32, pub items: Vec<GcRef> }
```
`record_equals` (records.rs:88-98) and `tuple_equals` (tuples.rs:78-92) stop
comparing leaked schema *addresses* — `Nominal(a)` vs `Nominal(b)` compares
keys, `Anonymous` vs `Anonymous` compares the ordered `(name, descriptor.id)`
sequence, mixed is false. `enum_format` (enums.rs:37-52) can finally render
`Wall` / `Number(3)` instead of `<variant 1: 3>`.

FOUR SYMPTOMS, ONE MISSING REPRESENTATION: TY-06 (`instantiate_walk` calls
`register_enum` per instantiation, minting a *fresh nominal def* each time —
generalize.rs:175-190, and the Record arm at :160-174 has the identical hole),
MONO-03 (the mono cache key is a *display string* from `db.render`,
mono.rs:148-150, and pretty.rs:111-114 renders enums by name only, so
`id[Option[Int]]` and `id[Option[Text]]` collide), RT-12/RT-13 (runtime records
compare by schema pointer; runtime enums carry no nominal identity at all),
MIR-12/DBG-06 (the schema cache is keyed on a bare, per-`TypeDb`-reusable `u32`
def id in a process-global map). Design `DefId + args` once and derive all
four.

**Replaces**

`TypeData::Record { def }` / `Enum { def }` and their ~30 pattern sites;
`register_enum` called from `instantiate_walk` (generalize.rs:175-190) and
per-annotation-site for `Option[T]` (infer.rs:1688-1697, synthesize.rs:98-104);
`MonoPass.cache: HashMap<(SymbolId, String), String>` (mono.rs:102, :148-150);
`record_equals`'s `pa.schema != pb.schema` (records.rs:96) and `tuple_equals`'s
(tuples.rs:86); `EnumPayload { tag, items }` (enums.rs:16-22) with no schema
pointer.

**Order and risk**

ORDER: tier 1 — after F5, F9; the runtime half additionally needs F1
(descriptor ids) and F13 (generation-scoped def keys).
`RecordSchema`/`EnumSchema` are `#[repr(C)]` and their addresses are embedded
as JIT immediates, so codegen and runtime MUST land in one commit and the ABI
version bumps with F6/F7/F18. `praxis_alloc_enum(ctx, tag, arity)` gains a
schema parameter. Judgement: threading a single canonical `Option` def-id from
`seed_builtin_schemes` (infer.rs:286-300) through the Inferer and the parser
lowerer; and whether compiler-synthesized structural enums are `Anonymous` or
get a stable key. Snapshot churn wherever pretty.rs starts printing
`Option[Int]` instead of `Option`.

#### F13 — praxis_codegen_cranelift::generation::Generation: one reclaimable JIT-generation arena

*Effort XL · crates: `praxis-codegen-cranelift`, `praxis-input-parser`, `praxis-hir`, `praxis-runtime`, `praxis-debugger` · unblocks MIR-12, MIR-13, DBG-05, DBG-06, IP-12, RT-12, RT-13, IP-07*

```rust
// crates/praxis-codegen-cranelift/src/generation.rs  (NEW)
#[derive(Clone,Copy,PartialEq,Eq,Hash,Debug)]
pub struct GenerationId(NonZeroU32);          // minted from a process AtomicU32

pub struct Generation {
    id: GenerationId,
    arena: bumpalo::Bump,
    record_schemas: HashMap<(GenerationId, RecordDefId), *const RecordSchema>,
    enum_schemas:   HashMap<(GenerationId, EnumDefId),   *const EnumSchema>,
    tuple_schemas:  HashMap<Box<[*const TypeDescriptor]>, *const TupleSchema>,
}
impl Generation {
    pub fn new() -> Generation;
    pub fn id(&self) -> GenerationId;
    pub fn alloc<T>(&self, v: T) -> &T;
    pub fn alloc_str(&self, s: &str) -> &str;
    pub fn alloc_slice<T: Copy>(&self, xs: &[T]) -> &[T];
}
pub struct Jit { module: JITModule, fn_ctx: FunctionBuilderContext, gen: Generation }
//   field order matters: `gen` must be declared AFTER `module` so it outlives the code.

// crates/praxis-input-parser/src/plan.rs — the parser's share of the same arena
pub struct PlanId(NonZeroU32);                 // 0 is NOT a plan → kills the `plan_index: 0` sentinel
pub struct TooManyPlans;
impl PlanArena {
    pub fn register(&mut self, plan: ParserPlan) -> Result<PlanId, TooManyPlans>;
    pub fn get(&self, id: PlanId) -> &ParserPlan;
}
```
Every `Box::leak` becomes `gen.alloc*`, returning `&'gen T`. `leak_static_str`
is DELETED so no code path can mint a `&'static` that outlives its generation.

THE JUDGEMENT CALL THE AUDIT UNDERSTATES: live `GcRef` payloads hold raw
`*const RecordSchema`/`*const TupleSchema`/`*const EnumSchema` into the arena,
so generation reclamation must be ordered strictly **after** heap teardown.
Encode it, do not document it: give `Generation` a `#[must_use] fn retire(self,
_proof: HeapDrained) -> ()` where `HeapDrained` is produced only by
`Runtime::teardown`, so 'reclaim a generation whose objects are still live'
does not type-check.

WHY THE KEY MUST CARRY THE GENERATION: `record_schema_for`'s cache
(lower.rs:1450-1486) is a process-global `OnceLock<Mutex<HashMap<u32,
SendPtr>>>` keyed on a bare `RecordDefId`, which is a *per-`TypeDb` positional
index*. The debugger mints a fresh `TypeDb` per `p` command
(evaluate.rs:234-249) and per `reload` (session.rs:116-153), so
`RecordDefId(0)` in session N names a different struct than in session N+1 and
the cache returns the wrong schema. That is a `u32` typed as if it were
absolute — the `(GenerationId, RecordDefId)` key is the fix, and it doubles as
the `SchemaIdentity::Nominal(u64)` key in F12.

**Replaces**

11 `Box::leak` sites in crates/praxis-codegen-cranelift/src/lower.rs (222,
1474, 1478, 1479, 1530, 1531, 1570, 1599, 1608, 1814, plus `leak_static_str`)
and 39 workspace-wide; the two process-global `OnceLock<Mutex<HashMap<…>>>`
schema caches at lower.rs:1464 and lower.rs:1520;
`praxis_input_parser::plan::PLAN_SLAB` (plan.rs:212) with its unbounded
`register_plan` push and unchecked `as u32` narrowing (plan.rs:215-221), `leak`
(plan.rs:181-184) and `leak_str` (plan.rs:353-358); and the `plan_index: 0`
failure sentinel at crates/praxis-hir/src/lower.rs:1727.

**Order and risk**

ORDER: tier 1, independent of the type-system foundations but a PREREQUISITE
for F12's runtime half (schema identity keys) and for anything that reclaims
across a debugger reload. Lifetime threading through `lower_function` (1
caller, module.rs:152) and the 6 functions containing leaks is mechanical but
wide; the `&'static` → `&'gen` change on `RecordSchema`/`DebugLocalMeta` fields
is *not* a runtime change (they are raw pointers) but IS a compile-time
reshuffle of every builder signature. `PlanId` propagates into praxis-runtime's
`praxis_run_parser`, which today recovers the index from a boxed Int payload
(ADR-023) — the reconstruction must be checked and out-of-range must become a
parse fault, not a panic. Do NOT let MIR-13, DBG-05 and IP-12 build three
separate arenas.

#### F14 — praxis_runtime::parser: absolute `Cursor`/`ByteRegion`/`Input` + validated `TextSlice`

*Effort XL · crates: `praxis-runtime`, `praxis-input-parser`, `praxis-hir` · unblocks IPR-01, IPR-02, IPR-03, IPR-04, IPR-05, IPR-06, IPR-07, IPR-08, IPR-09, IPR-10, IPR-11, IPR-12, RT-06, IP-10*

```rust
// crates/praxis-runtime/src/parser/cursor.rs   (new module inside parser.rs)
/// An ABSOLUTE byte offset into the one input buffer. No public `usize`
/// constructor, so `bytes.len() - offset` is not expressible.
#[derive(Clone,Copy,PartialEq,Eq,PartialOrd,Ord,Debug)]
pub(crate) struct Cursor(usize);
impl Cursor { fn advance(self, n: usize) -> Cursor; fn offset(self) -> usize; }

/// The single borrow of the input buffer, carrying its owning `GcRef`.
/// Built ONCE in `run_plan` from the `input: GcRef` argument.
pub(crate) struct Input<'a> { owner: GcRef, buf: &'a [u8], text: &'a str }

/// An absolute `[start, end)` window. `subregion` can only NARROW.
#[derive(Clone,Copy)]
pub(crate) struct ByteRegion { owner: GcRef, start: Cursor, end: Cursor }
impl ByteRegion {
    fn bytes<'a>(&self, i: &Input<'a>) -> &'a [u8];
    fn str<'a>(&self, i: &Input<'a>) -> &'a str;                  // scalar-aligned by construction
    fn subregion(&self, s: Cursor, e: Cursor) -> ByteRegion;      // debug_asserts containment
    fn next_scalar(&self, i: &Input, at: Cursor) -> Option<Cursor>;
}
/// invariant: region.start <= next <= region.end
pub(crate) struct Walked { value: GcRef, next: Cursor }
type WalkResult = Result<Walked, ParseFail>;

/// Full-consumption variant. Errors unless `next == region.end`, and returns a
/// bare `GcRef` — so a caller structurally CANNOT forget the check.
fn walk_exact(rt: &Rt, i: &Input, plan: &ParserPlan, node: u32, region: ByteRegion)
    -> Result<GcRef, ParseFail>;

// crates/praxis-runtime/src/text.rs — the validated slice constructor
impl TextPayload {
    /// The ONLY constructor of `TextPayload::Slice`. Validates
    /// `start + len <= owner_bytes.len()` and that both ends are char boundaries.
    pub fn slice(owner: GcRef, start: usize, len: usize) -> Option<TextPayload>;
}
// crates/praxis-runtime/src/context.rs
impl Runtime { pub fn alloc_text_slice(&self, owner: GcRef, start: usize, len: usize) -> Option<GcRef>; }

// crates/praxis-input-parser — separator non-emptiness, by construction
pub struct Separator(Box<str>);
impl Separator { pub fn new(s: &str) -> Result<Separator, ValidationError>; }  // rejects ""
```
With `TextPayload::slice` validated, `text_str` (text.rs:83-89) drops its
`unwrap_or("")` silent-empty and `text_bytes` (text.rs:58-76) drops its silent
clamp — and note `text_bytes` clamps `end` but NOT `start`, so an out-of-range
start is currently an index panic across `extern "C"`.

WHY ONE FOUNDATION AND NOT TWELVE FIXES: `WalkResult = Result<(GcRef, usize),
ParseFail>` (parser.rs:100) is documented as 'bytes consumed' but 12 sites
return an absolute offset and 5 sites re-slice `&bytes[a..b]` and then walk at
offset 0 with the ORIGINAL owner (parser.rs:333/375/385/683/706). Every one of
IPR-01 … IPR-10 is a consequence. `walk_exact` plus a narrowing-only
`ByteRegion` closes the unbounded-child-parser family (IPR-02/04/05/06/09) in
one shape, and `next_scalar` closes the byte-vs-scalar family (IPR-06/08).
`Separator` non-emptiness closes both the missing validation (IP-10) and the
non-advancing `walk_sep` loop (parser.rs:790-822) with no runtime check.

**Replaces**

`type WalkResult = Result<(GcRef, usize), ParseFail>` (parser.rs:100) and all
31 walk helpers + 20 recursive `walk(rt.ctx, …)` call sites; the
`region_offset_of` substring search (parser.rs:1277-1283, which panics on
`windows(0)`); the `rt_owner`/`ctx.input_source` owner re-fetch
(parser.rs:298); the five `&bytes[a..b]`-then-offset-0 re-slices;
`str::from_utf8(..).unwrap_or("")` at parser.rs:804 and :669; and `Sep {
separator: String }` (ast.rs:123-128) plus its `_ => String::new()` laundering
at parser_lower.rs:419.

**Order and risk**

ORDER: tier 1, independent of the type-system foundations. `walk`/`WalkResult`
are private and the only public entry is `run_plan_by_index` (parser.rs:23)
called from abi.rs:2904, so the blast radius is one 1608-line file plus 3
`alloc_text_slice` call sites. Mechanical for the 12 relative-return sites;
JUDGEMENT for the 5 re-slicing sites and for whether `grid(int)` means one
digit per cell. Behaviour changes visible to users: previously-accepted inputs
will now fault (correctly, §7.5) — re-check
crates/praxis-codegen-cranelift/tests/jit.rs:872/883/904/2143/3866/3877/3979/3989/3998.
DO NOT run the `sep("", P)` regression in a shared test process before
`Separator` lands (infinite loop). IPR-14 (rooting the interpreter's
`Vec<GcRef>` intermediates) rides on F7 and must come AFTER it, not with this.

#### F15 — praxis_hir: per-node inferred-type map (`NodeKey` → `Type`) + `MethodRef`

*Effort XL · crates: `praxis-hir`, `praxis-mir`, `praxis-debugger` · unblocks HIR-01, HIR-02, HIR-09, MONO-01, MONO-02, P0-02, MIR-16, DBG-02, TY-30*

```rust
// crates/praxis-hir/src/lib.rs
/// A node identity. Distinct TYPE from a token `TextRange`, so the existing
/// PATH_EXPR-node vs Ident-token range collision with `ref_types` is unrepresentable.
#[derive(Clone,Copy,PartialEq,Eq,Hash,Debug)]
pub struct NodeKey(rowan::TextRange, praxis_syntax::SyntaxKind);
impl NodeKey { pub fn of(n: &praxis_syntax::SyntaxNode) -> NodeKey; }

pub struct MethodRef { pub entry: &'static praxis_stdlib::MethodEntry,
                       pub receiver: Type, pub result: Type }
pub struct CallSite  { pub callee: SymbolId, pub arg_types: Vec<Type>,
                       pub result: Type }          // NEW field — the result witness

pub struct Analysis {
    …,
    /// EVERY inferred expression's type. One insertion point, so "an inferred
    /// expression with no recorded type" cannot arise.
    pub expr_types: HashMap<NodeKey, Type>,
    /// Method references, keyed by the method-name token. Replaces smuggling a
    /// method's result through `ref_types` at a token that is not a name reference.
    pub method_refs: HashMap<rowan::TextRange, MethodRef>,
}
```
One insertion at the tail of `Inferer::infer_expr` (infer.rs:675): `let t =
match expr {…}; self.expr_types.insert(NodeKey::of(expr.syntax()), t); t`.
`Inference` (infer.rs:40-60) gains the same two fields.

Lowering becomes a pure reader. Delete `symbol_type` (lower.rs:2005),
`call_result_type` (lower.rs:2014), `param_type` (lower.rs:802), the 5
`db.instantiate` re-instantiations (lower.rs:723, 808, 1760, 2009, 2017) and
**all 19 `db.fresh_var()` fallbacks** — a miss becomes
`DiagCode::InternalMissingType`, never a fresh var, because a fresh var is
exactly the silent lie the audit is tracking. Post-condition: a debug walk of
`TypedModule` asserting no `TypedExpr.ty` resolves to `VarState::Unbound`
unless inference itself failed.

WHY THIS IS THE HIGHEST-LEVERAGE HIR FOUNDATION: ADR-014 says lowering may
're-derive in read mode'. It does not re-derive — it *re-infers*, at a second,
independent instantiation, which is why `lower_method_call`
(lower.rs:1627-1662) can emit `Y110` and `Unit` for a method inference already
resolved. It is also the direct supplier for ~35 of P0-02's `Type(0)` sites:
`alloc_gc` calls in build.rs pass `Type(0)` precisely where `TypedExpr.ty` is a
fresh var, and several (pipeline accumulators build.rs:2409/2427/2439,
`source_item_ty` build.rs:1863) have no correct type available at all until
this map exists.

**Replaces**

Lowering's re-derivation layer: crates/praxis-hir/src/lower.rs:2005
`symbol_type`, :2014 `call_result_type`, :802 `param_type`, :721-728
`lower_fn`'s independent `instantiate`, :1539/:1629 `lower_method_call`'s
second catalog lookup, :1760/:2133; and the 19 `db.fresh_var()` fallbacks. Also
replaces `ref_types.insert` at infer.rs:1437-1440 (a method token is not a name
reference) and the pattern of reading a capture's type off a *reference site*
rather than its binding (capture.rs:74/83/211, lower.rs:1069-1073).

**Order and risk**

ORDER: tier 2 — after F10 (Scheme/constraints) and the block/if/return
inference fixes it depends on (TY-16, TY-17, TY-18), since it records whatever
inference computes and would otherwise freeze today's wrong block/if types into
MIR. MIR consumes only `TypedExpr.*.ty` (75 `TypedExpr::` matches in build.rs),
so NO MIR signature changes — but MIR *behaviour* changes everywhere a
descriptor is chosen, which is the point and also the risk. Judgement: which
node kinds inference visits vs which lowering constructs — `Paren` transparency
(lower.rs:935-942), `Expr::Error` (lower.rs:971), synthesized Unit tails
(lower.rs:841) have no inferred node and need an explicit rule.

#### F16 — praxis_mir::ir::MirType + raw-word elimination (`LoadCapture`, `TupleShape`)

*Effort XL · crates: `praxis-mir`, `praxis-codegen-cranelift`, `praxis-debugger` · unblocks P0-02, P0-03, MIR-05, MIR-16, P0-11, DBG-01, DBG-02, MIR-10*

```rust
// crates/praxis-mir/src/ir.rs
#[derive(Clone,Copy,PartialEq,Eq,Debug)]
pub enum MirType { Known(praxis_types::Type), Opaque }
impl MirType {
    pub fn known(self) -> Option<Type>;
    pub fn expect_known(self, at: &'static str) -> Result<Type, MirTypeError>;
}
pub struct Local { pub id: LocalId, pub kind: LocalKind, pub ty: MirType }
impl Function {
    pub fn new_local(&mut self, kind: LocalKind, ty: MirType, debug_name: Option<String>,
                     debug_kind: LocalDebugKind, debug_span: Option<(u32,u32)>) -> LocalId;
}

/// Tuple element types must be `Known` — a tuple whose element descriptors are
/// unknown cannot produce a `TupleSchema`. Arity < 2 is unrepresentable.
pub struct TupleShape(Box<[praxis_types::Type]>);
impl TupleShape { pub fn new(elems: Vec<Type>) -> Option<TupleShape>; }
pub enum AllocKind { …,
    Tuple { shape: TupleShape, elements: Vec<LocalId> },   // WAS Tuple { ty: Type, elements }
}

/// Raw ABI words become instruction IMMEDIATES, following the existing
/// `Inst::LoadField { field_idx: u32 }` precedent — NOT a third `LocalKind`.
pub enum Inst { …,
    LoadCapture { dst: LocalId, closure: LocalId, index: u32 },   // NEW
}
// `MoveGc` is narrowed (doc + verifier, F17) to Gc→Gc; `Materialize` becomes the
// ONLY Scalar→Gc transition.
```
`AllocKind::Collection { ctor, args }` (which already resolves descriptors in
the backend at lower.rs:670+) replaces the hand-rolled null-descriptor `Call`
in `alloc_empty_vec` (build.rs:3038-3060) — the second and last site where a
raw integer inhabits a `LocalKind::Gc` slot.

TWO CORRECTIONS TO THE AUDIT:
(1) It proposes `AllocKind::Tuple { schema: Vec<MirType> }`. That is wrong: an `Opaque` element type cannot yield an element descriptor and codegen would be back to a fallback. Element types must be `Known` by construction (`Vec<Type>`), and a builder without a real type must not emit the `Tuple` at all. `TupleShape::new` rejecting arity < 2 also fixes MIR-05's zero-field-tuple regression at the *constructor* rather than inside `tuple_schema_for` (lower.rs:1498).
(2) `LoadCapture` must NOT be listed in `safepoint_roots_slot` (liveness.rs:253): `praxis_closure_capture` (abi.rs:1153-1165) neither allocates nor sets a fault, so the `b.check_fault()` at build.rs:406 is vacuous today and should be dropped, not carried over.

**Replaces**

40 `Type(0)` literals in crates/praxis-mir/src/build.rs (scalar placeholders at
:459 become legitimately `Opaque`; the ~35 Gc sites take the real type from
`expr_types`, F15); `Local.ty: Type` (ir.rs:80); `AllocKind::Tuple { ty: Type
}` (ir.rs:302) and the `tuple_schema_for` degrade-to-arity-0 path
(lower.rs:1498-1537); and the ConstInt+MoveGc+Call closure-capture idiom at
build.rs:388-408 plus the null-descriptor idiom at build.rs:3038-3060 — the two
remaining scalar→Gc edges.

**Order and risk**

ORDER: tier 2 — REQUIRES F5 (sealed `Type`, so `Type(0)` stops being writable),
F11 (`descriptor_for_type -> Result`, which is what forces `Known` at the
boundary) and F15 (which supplies the real types for ~35 sites). Blast radius:
75 `alloc_gc(` + 2 `alloc_temp(` in build.rs, 7 `new_local(` callers, 14 `.ty`
readers across praxis-mir + praxis-codegen-cranelift, 10 `AllocKind::Tuple`
references; adding `Inst::LoadCapture` touches 4 exhaustive matches (ir.rs,
liveness.rs:144-164 defs, :200-220 uses, lower.rs `lower_inst`) with exactly 1
producer. NOT mechanical: ~35 sites need a judgement call on which real type to
supply, and `build_debug_local_metas` (lower.rs:1588) must emit a
no-static-type `DebugLocalMeta` for `Opaque` rather than a wrong `type_id` —
see F18.

#### F17 — praxis_mir: `RootSlots`/`DebugSlots` newtypes + effect-driven safepoints + `verify` pass

*Effort L · crates: `praxis-mir`, `praxis-codegen-cranelift`, `praxis-cli`, `praxis-debugger` · unblocks MIR-01, MIR-02, MIR-10, MIR-11, MIR-16, P0-03, P0-08, P0-02*

```rust
// crates/praxis-mir/src/annot.rs  (NEW)
/// The GC root set at a safepoint. NO public constructor: only
/// `liveness::annotate` may fill one.
#[derive(Debug, Default)]
pub struct RootSlots(Vec<LocalId>);
impl RootSlots {
    pub fn unannotated() -> RootSlots;             // what every builder writes
    pub fn iter(&self) -> impl Iterator<Item = LocalId> + '_;
    pub fn is_annotated(&self) -> bool;
    pub(crate) fn set(&mut self, ids: Vec<LocalId>);
}
/// SEPARATE from RootSlots: what the debugger must see (ADR-021/-035).
/// Deliberately over-approximate; shrinking RootSlots must not shrink this.
#[derive(Debug, Default)] pub struct DebugSlots(Vec<LocalId>);

// crates/praxis-mir/src/ir.rs — every `live_roots: Vec<LocalId>` becomes:
Inst::Alloc { dst, alloc, roots: RootSlots },
Inst::Call  { dst, callee: CallTarget, args, roots: RootSlots },     // callee.effect() decides
Inst::CheckFault { on_fault: BlockId, debug: DebugSlots },           // NOT a GC safepoint

// crates/praxis-mir/src/verify.rs  (NEW)
pub enum VerifyError {
    RootIsNotGc { func: String, block: BlockId, inst: usize, local: LocalId },
    MoveGcFromScalar { .. },                 // P0-03: the last scalar→Gc edge
    ScalarLiveAcrossSafepoint { .. },        // ADR-015 §10.3
    OpaqueAtDescriptorSite { .. },           // P0-02: MirType::Opaque feeding a descriptor
    UnannotatedSafepoint { .. },             // effect().allocates() && !roots.is_annotated()
    AllocatingCallNotASafepoint { .. },      // the ABI manifest disagrees with the Inst shape
    BadBlockTarget { .. }, MissingTerminator { .. },   // MIR-11 class
}
pub fn verify(f: &Function) -> Result<(), Vec<VerifyError>>;
```
`liveness::annotate`'s two-pass forward/backward walk (liveness.rs:73-92, whose
`apply_forward` at :236 never kills a def and so over-approximates
monotonically) collapses to one backward walk reusing the pass-1 transfer; the
roots at an instruction are `((live_out \ defs) ∪ uses) ∩ gc_locals`.

THE SEQUENCING CONSTRAINT THE AUDIT CORRECTLY FLAGS AND THE PLANNER MUST
HONOUR: today ONE `live_roots` field and ONE `emit_spill` (lower.rs:399-441)
serve both the ShadowFrame and the DebugFrame. MIR-01 (null dead slots) and
MIR-02 (shrink the set) regress crates/praxis-cli/tests/run.rs:338 and :372
unless `DebugSlots` lands FIRST. Split the sets, then shrink the roots.

WHY `verify` is a foundation and not a nicety: with `RootSlots` sealed and
`Effect` from F4, the two illegal states (a scalar in the root set, an
allocating call with no root set) become unconstructible — and `verify` is what
keeps P0-02 and P0-03 from regressing silently once fixed. It also catches
MIR-11's `lower_for` continue-target bug (build.rs:1534) for free.

**Replaces**

61 hand-written `live_roots: vec![…]` / `live_roots: Vec::new()` literals in
crates/praxis-mir/src/build.rs, every one of which `liveness::annotate`
silently overwrites; `safepoint_roots_slot`'s hand-maintained variant list
(liveness.rs:247-263), which omits `IntBinOp` even though its codegen arm emits
two `praxis_alloc_int` calls (P0-08); `apply_forward` and the pass-2 forward
walk (liveness.rs:73-92, :236); and the absence of any post-`annotate`
validation at the 4 pipeline sites (praxis-cli/src/run.rs:110,
adversarial_audit.rs:49, praxis-debugger/src/session.rs:136, evaluate.rs:244).

**Order and risk**

ORDER: tier 2 — REQUIRES F4 (Effect on `CallTarget::Runtime`) and F16
(`MirType`, so `OpaqueAtDescriptorSite` is expressible). Land `DebugSlots`
before any root-set shrinking. Adding the ScalarLiveAcrossSafepoint check will
initially FAIL on the eager `lower_seq_*` lowerers (build.rs:2984-3033,
3172-3255) whose accumulators are scalar across calls — either move them into
Gc slots or scope the check, and decide that explicitly rather than weakening
the verifier. `RootSlots` deletion of the 61 literals is mechanical; the
liveness rewrite (~30 lines) is judgement.

#### F18 — praxis_runtime::debug: `Option<GcRef>` debug values + optional static type

*Effort M · crates: `praxis-runtime`, `praxis-codegen-cranelift`, `praxis-debugger`, `praxis-cli` · unblocks P0-04, MIR-09, MIR-16, MIR-01, DBG-02, P0-02*

```rust
// crates/praxis-runtime/src/debug.rs
#[repr(C)]
pub struct DebugLocal {
    /// `None` is the all-zero niche of `NonNull`, so an uninitialized slot and a
    /// live value are DIFFERENT VALUES, not the same pointer compared by identity.
    pub value: Option<GcRef>,
    …
}
#[repr(C)]
pub struct DebugLocalMeta {
    pub name_ptr: *const u8, pub name_len: u32, pub kind: u8,
    /// `NO_STATIC_TYPE` (== u32::MAX) when the MIR local's `ty` is
    /// `MirType::Opaque`. Was a forged `Type(0)` arena index.
    pub type_id: u32,
    pub span_start: u32, pub span_end: u32,
}
pub const NO_STATIC_TYPE: u32 = u32::MAX;
```
`crash_snapshot.rs:242-248`'s `is_real_ref`/`null_sentinel_ref` — a
`NonNull::dangling()` compared by pointer identity to decide whether a slot
holds a value — is DELETED. The codegen spill (lower.rs:399-441) writes
`Some(v)`; dead slots are written `None` rather than left stale. The debugger
renders `<uninit>` from `None` and `<unknown type>` from `NO_STATIC_TYPE`,
instead of inventing `Int`.

PAIRED, SAME COMMIT: every generated fault epilogue must return
`RuntimeContext.unit_ref`, not `iconst 0` (lower.rs:316-317, :1175-1176). Add
```rust
// crates/praxis-codegen-cranelift/src/lower.rs
const UNIT_REF_OFFSET: i64 = core::mem::offset_of!(RuntimeContext, unit_ref) as i64;
fn emit_unit_return(builder: &mut FunctionBuilder, ctx_val: Value);   // the ONLY producer
```
so a third exit path cannot reinvent the integer-zero return.
`RuntimeContext::placeholder` (context.rs:256-271) already sets `unit_ref:
input_source`, so the sentinel is non-null even for unwired test scaffolding.

WHY THESE TWO TOGETHER: both are instances of one class — 'an invalid `GcRef`
bit pattern is representable in a slot typed as a valid one'. `GcRef` is
`#[repr(transparent)]` over `NonNull`, so constructing either the fault
epilogue's zero or the debug frame's dangling sentinel as a Rust `GcRef` is UB.
`Option<GcRef>` is niche-optimized to the same ABI word, so both fixes are free
at runtime and both make the invalid state unrepresentable in Rust. MIR-09's
regression asserts the fault path returns the Unit sentinel, so P0-04 gates it.

**Replaces**

crates/praxis-runtime/src/debug.rs:20-31 `DebugLocal.value: GcRef`;
crash_snapshot.rs:242-248 `is_real_ref`/`null_sentinel_ref`;
`DebugLocalMeta.type_id` populated from a forged `Type(0)`
(crates/praxis-codegen-cranelift/src/lower.rs:1588-1595); the two `iconst(GC,
0)` fault returns at lower.rs:316-317 and :1175-1176 and their misleading
comments at :299-301 and :1173-1174; and `null_sentinel()` at
crates/praxis-debugger/src/evaluate.rs:358-363 (dead once F4 removes the
phantom `RunnableFunction` parameter at module.rs:25).

**Order and risk**

ORDER: tier 2 — after F16 (`MirType::Opaque` is what `NO_STATIC_TYPE` encodes)
and F17 (`DebugSlots` decides which locals are spilled at all).
`DebugLocal`/`DebugLocalMeta` are `#[repr(C)]` and read by generated code;
`DEBUG_VALUE_OFFSET`/`DEBUG_LOCAL_SIZE` (lower.rs:390-392) recompute
automatically via `offset_of!`, but bump the ABI version — batched with F6 and
F7 as ONE bump. Two CLI tests, crates/praxis-cli/tests/run.rs:338 and :372,
currently specify debugger fidelity that depends on the over-approximate root
set; re-specify them here rather than letting F17 break them. The P0-04 half
alone is 2 lines + 1 const and is the cheapest P0 in the whole audit — but note
it does NOT close P0-08's `praxis_int_load` on the sentinel: after this fix
that path reads the real Unit object rather than address 0, turning a segfault
into a wrong-descriptor read.

#### F19 — praxis_hir::decl: sealed `TypeEnv` + `DeclGroup` two-phase inference driver

*Effort L · crates: `praxis-hir`, `praxis-types` · unblocks TY-01, TY-09, TY-10, TY-11, TY-13, TY-14, TY-15, TY-22, TY-23, TY-24, HIR-03*

```rust
// crates/praxis-hir/src/decl.rs   (NEW)
/// Built and SEALED before any expression is inferred. Total for every name the
/// resolver accepted, so "a registered type name with no Type" is unreachable
/// during expression inference.
pub struct TypeEnv { types: HashMap<SymbolId, Type>, signatures: HashMap<SymbolId, Type> }
impl TypeEnv {
    pub fn ty(&self, s: SymbolId) -> Option<Type>;          // type names (struct/enum)
    pub fn signature(&self, s: SymbolId) -> Option<Type>;   // fn signatures (forward calls)
}

/// One inference level, opened and closed as a unit. The ONLY minter of a
/// recursive-fn placeholder, so a placeholder created at the wrong level is
/// unconstructible (ADR-008 / TY-01).
pub struct DeclGroup<'db> { db: &'db mut TypeDb, saved: Level, members: Vec<(SymbolId, Type)> }
impl<'db> DeclGroup<'db> {
    pub fn open(db: &'db mut TypeDb) -> DeclGroup<'db>;     // enter_level
    pub fn placeholder(&mut self, s: SymbolId) -> Type;     // minted AT the body level
    pub fn close(self) -> Vec<(SymbolId, Scheme)>;          // exit_level + generalize all
}

/// Phase A: type declarations (fixpoint for mutually recursive structs/enums).
/// Phase B: fn signatures from written annotations, one SCC at a time.
pub fn declaration_pass(root: &SourceFile, res: &NameResolution, db: &mut TypeDb)
    -> (TypeEnv, Vec<Scc<SymbolId>>);

// crates/praxis-hir/src/scope.rs — honour the doc that already promises this
impl ScopeTree { pub fn bind(&mut self, s: ScopeId, name: &str, id: SymbolId) -> Option<SymbolId>; }
//   returns the DISPLACED symbol; `register_top_level` diagnoses a displaced Fn/Struct/Enum
//   (DiagCode::DuplicateDeclaration) and ignores displacement for let/var (§4.2 shadowing).

// dispatcher split — a nested item cannot reach the item path at all
fn resolve_top_stmt(..);  fn resolve_block_stmt(..);
fn infer_top_stmt(..);    fn infer_block_stmt(..);
```
With `TypeEnv` sealed, `Inferer` DROPS its `scopes: ScopeTree` field
(infer.rs:105) and the vestigial `scope: ScopeId` parameter threaded through 34
methods (infer.rs:1022 literally has `let _ = scope;`). `ScopeTree` becomes
owned exclusively by the resolver, so 'inference consults a scope the resolver
never populated' — the root cause of TY-13's shadowing bug in `infer_assign`
(infer.rs:645) and of HIR-03's constructor-by-text lookup (lower.rs:1576) — is
unrepresentable. Every pass reads bindings from the range-keyed `refs`/`decls`
maps, which lower.rs:730-734 already documents as the correct rule.

SEVEN FINDINGS, ONE PASS: TY-01(c) needs the SCC group for placeholder levels;
TY-22 needs the same signature pre-pass (and the audit's own TY-01 note —
'correcting `lower_levels` requires moving recursive placeholders to the group
level' — IS this pass); TY-10 needs the type-declaration pre-pass; TY-09 needs
`TypeEnv` to be total; TY-24 needs `bind`'s return value; TY-23 needs the
dispatcher split; TY-13 needs `ScopeTree` out of `Inferer`.

**Replaces**

crates/praxis-hir/src/infer.rs:83-85 (a single flat source-order loop over
top-level statements, so a struct used before declaration and a call to a later
fn both silently get fresh vars); infer.rs:558-574 (the recursive placeholder
minted BEFORE `enter_level`, which pulls params/results to level 0) and
:617-621 (the fn scheme overwritten after the body); infer.rs:378-387/:448-457
(`lookup_struct_type`/`lookup_enum_type`, near-duplicates gated on
`sym.scheme.as_ref()?`, the second dead); infer.rs:1361-1408 (`infer_call`'s
silent fresh-var fallthrough for a signature-less `SymbolKind::Fn`);
crates/praxis-hir/src/scope.rs:69-78 (`bind` silently overwrites while its doc
promises to return the displaced symbol); and the `Inferer.scopes` field plus
34 `scope: ScopeId` parameters.

**Order and risk**

ORDER: tier 2 — after F10 (Level newtype, scheme-owned binders). TY-01's
`clamp_levels` fix and TY-22's signature pre-pass MUST land together: fixing
the reversed comparison at unify.rs:76 without moving the placeholder to the
group level will unsoundly generalize every pre-declared signature. Confined to
praxis-hir (+ `Level` in praxis-types); `ScopeTree`/`ScopeId` are re-exported
at lib.rs:39 but have ZERO consumers outside the crate. Judgement-heavy: mutual
recursion in phase A (`struct A { b: B } struct B { a: A }`) needs a
declare-then-define two-phase or a fixpoint; SCC construction interacts with
shadowing and with nested `fn` items reached via infer.rs:1207. Expect wide
infer_tests/hover_tests snapshot churn — any inferred scheme that changes moves
a snapshot. Downstream, mono.rs starts seeing call sites for forward calls it
never saw.

#### F20 — praxis_hir::TypedExpr: one derived child walker

*Effort S · crates: `praxis-hir`, `praxis-mir` · unblocks HIR-08, MONO-01, MONO-02, TY-17, TY-21, HIR-09*

```rust
// crates/praxis-hir/src/lower.rs
impl TypedExpr {
    pub fn children(&self) -> impl Iterator<Item = &TypedExpr>;
    pub fn children_mut(&mut self) -> impl Iterator<Item = &mut TypedExpr>;
    pub fn blocks(&self) -> impl Iterator<Item = &TypedBlock>;
    pub fn blocks_mut(&mut self) -> impl Iterator<Item = &mut TypedBlock>;
}
```
Written ONCE — ideally by a `macro_rules!` that lists each of the 29 variants'
child fields exactly once, so adding a field to a variant without listing it is
a compile error rather than a silent omission in three places. All three
existing walkers become folds over it.

EVIDENCE THAT DRIFT IS THE ACTUAL FAILURE MODE, NOT A HYPOTHETICAL:
`collect_escaping_expr` (lower.rs:545) destructures `TypedExpr::Call` at
lower.rs:598 and omits `callee_expr` — so a `var` captured by a closure that is
*itself* the callee of a call is never marked escaping, `CaptureKind::ByCell`
(lower.rs:1074) is not selected, and the mutation is lost (HIR-08).
`mono.rs:199 rewrite_expr` and `praxis-mir/src/build.rs collect_closures_expr`
enumerate the same 29 arms independently and can each drop a field the same
way. Any new variant (TY-17's `IfValue`/`IfEffect` split, TY-21's `Loop`
reshape, TY-15's `CompoundAssign`) triples that risk.

Secondary and stronger for HIR-08 specifically: derive `escaping_vars` from the
captures the lowerer already holds in hand at lower.rs:1060-1086, rather than
re-walking the tree afterwards — then the walker is irrelevant to that
correctness property at all. `CaptureKind::ByCell` and membership in
`escaping_vars` are two representations of one fact; keep one.

**Replaces**

crates/praxis-hir/src/lower.rs:545-620 `collect_escaping_expr`,
crates/praxis-hir/src/mono.rs:199-300 `rewrite_expr`, and
crates/praxis-mir/src/build.rs `collect_closures_expr` — three hand-written
~29-arm walks over the same enum, each independently forgettable (~75 arms
total).

**Order and risk**

ORDER: tier 2, but effectively free and worth landing BEFORE TY-17/TY-21 add
`TypedExpr` variants — otherwise each new variant must be added to three
walkers by hand. Purely mechanical. `escaping_vars` is consumed only by
crates/praxis-mir/src/build.rs. The one-line minimal version of HIR-08 (add
`callee_expr` to the destructure at lower.rs:598) should NOT be shipped alone —
it fixes the instance and leaves the class.

#### F21 — `Option[T]` as a real prelude type: `TypePattern::Option` + prelude `EnumSchema` + honest `PreludeBinding`

*Effort L · crates: `praxis-stdlib`, `praxis-types`, `praxis-runtime`, `praxis-hir`, `praxis-mir`, `praxis-codegen-cranelift` · unblocks RT-14, RT-15, TY-33, TY-34, RT-13, TY-06, IP-06*

```rust
// crates/praxis-stdlib/src/type_pattern.rs
pub enum TypePattern { …, Option(Box<TypePattern>) }

// crates/praxis-stdlib/src/prelude.rs — a prelude name that cannot be typed and
// lowered becomes unrepresentable: there is deliberately NO `Unimplemented` variant.
pub enum PreludeBinding { Scheme(SchemeSpec), CollectionCtor(CollectionCtor),
                          EnumCtor(EnumSpec), TypeOnly }
pub struct PreludeEntry { pub name: &'static str, pub doc: &'static str,
                          pub binding: PreludeBinding }

// crates/praxis-types/src/db.rs — ONE canonical Option def (see F12)
impl TypeDb { pub fn option_def(&self) -> EnumDefId; pub fn option_of(&mut self, t: Type) -> Type; }

// crates/praxis-runtime/src/enums.rs — the runtime side
/// Some == tag 0 (1 item), None == tag 1 (0 items), against ONE prelude schema.
pub static OPTION_SCHEMA: EnumSchema;
pub fn some(rt: &Runtime, v: GcRef) -> GcRef;
pub fn none(rt: &Runtime) -> GcRef;
```
`praxis_map_get` (abi.rs:1609-1615) returns `some(v)`/`none()`;
`praxis_grid_find` (abi.rs:2588-2609) likewise; their catalog entries become
`result: TypePattern::Option(...)`.

THE INVARIANT TEST THAT WOULD HAVE CAUGHT BOTH: for every `MethodEntry` with
`can_fault: false` and a non-`Unit` result, assert the runtime symbol provably
cannot return the Unit sentinel. `rg 'unit_sentinel'
crates/praxis-runtime/src/abi.rs` = 56 hits; most are legitimate `-> Unit`
methods, but each one whose catalog `result` is not `TypePattern::Unit` is the
same defect as RT-14/RT-15. Add that test with this foundation, not after.

WHY IT IS A FOUNDATION: `Option[T]` already exists as a *registered enum*
(infer.rs:286-300, 1688-1697) but has no `TypePattern`, no `CollectionCtor`, no
canonical def-id, and no runtime schema — so today `m.get(k)` is statically
typed `V` and dynamically returns the Unit sentinel, which is exactly 'a value
whose static type is V and whose runtime descriptor is UNIT'. §4.7 makes Option
the absence channel and reserves faulting for `map[key]` indexing, so this is
not a design choice left open. It is also the prerequisite for the `TY-33`
prelude cleanup: `PreludeEntry` currently lists 15 names with no scheme and no
lowering (including `panic`, which typechecks and then fails to compile), and
`seed_builtin_schemes`'s `other => panic!("unexpected builtin")` at
infer.rs:308 is itself a latent violation of the no-panic contract at
lib.rs:96-113.

**Replaces**

crates/praxis-runtime/src/abi.rs:1609-1615 (`praxis_map_get` returning the Unit
sentinel for a missing key) and :2588-2609 (`praxis_grid_find`);
crates/praxis-stdlib/src/builtins.rs:643-657 (`map_get` catalog `result:
Var("V")`) and :1317-1330 (`grid_find`); the fresh `register_enum` per
`Option[T]` annotation site at infer.rs:1688-1697 and synthesize.rs:98-104;
`PreludeEntry { name, doc }` (prelude.rs:62-73) with `scheme: None` for all 33
entries (resolve.rs:193-199) and the hardcoded string filter at
infer.rs:213-233 whose `other => panic!` is unreachable-by-construction
afterwards.

**Order and risk**

ORDER: tier 2 — REQUIRES F12 (nominal identity + `EnumSchema`) and F1.
SOURCE-VISIBLE LANGUAGE CHANGE: existing programs using `m.get(k)` as a bare
`V` stop compiling; the AoC corpus and crates/praxis-cli/tests must be triaged.
Adding `TypePattern::Option` forces an arm in every `match` over `TypePattern`
in praxis-stdlib and praxis-hir, and MIR must lower `match m.get(k) { Some(v)
=> … None => … }`. `PRELUDE` has exactly one consumer (resolve.rs:194) so the
struct change is cheap, but the remove-vs-implement decision for the 15 phantom
names (implement `panic`/`assert`/`abs`/`sign`/`min`/`max`/`clamp`/`gcd`/`lcm`;
DELETE the six graph helpers
`bfs`/`bfs_distance`/`dfs`/`dijkstra`/`a_star`/`flood_fill` until a milestone
owns them) is a PRODUCT judgement and must be an explicit planner decision, not
left to the implementer. `panic` must land first — it already has a scheme and
a `Never` result, so `panic("x")` typechecks today and then fails at JIT time
with `unresolved user function`.

## 4. Finding register

Every finding with its verified severity, owning stage, and subsystem.
Per-finding evidence, code sites, fix specification, gating tests and blast
radius are recorded in the verification data behind this plan; this table is
the index.

**§4.1 is a second register**, for defects found *while executing this plan*
rather than by the audit. They are scheduled the same way and their stages are
real; what differs is provenance, and that is why they are not mixed in below.

| ID | Sev | Status | Effort | Stage | Subsystem |
|---|---|---|---|---|---|
| P0-01 | P0 | CONFIRMED | M | S1 | p0-identity |
| P0-05 | P0 | CONFIRMED | S | S2 | p0-rooting |
| P0-02 | P0 | CONFIRMED | XL | S3 | p0-identity |
| P0-03 | P0 | CONFIRMED | M | S3 | p0-identity |
| P0-04 | P0 | CONFIRMED | S | S3 | p0-identity |
| P0-08 | P0 | CONFIRMED | L | S3 | p0-rooting |
| P0-06 | P0 | CONFIRMED | L | S5 | p0-rooting |
| P0-07 | P0 | CONFIRMED | XL | S5 | p0-rooting |
| P0-11 | P0 | CONFIRMED | L | S7 | p0-layout |
| RT-06 | P0 | CONFIRMED | M | S7 | rt |
| RT-09 | P0 | CONFIRMED | S | S7 | rt |
| MIR-01 | P0 | CONFIRMED | L | S9 | mir |
| MIR-09 | P0 | CONFIRMED | M | S9 | mir |
| P0-12 | P0 | CONFIRMED | L | S10 | p0-layout |
| IPR-03 | P0 | CONFIRMED | M | S20 | ip-runtime |
| FE-01 | P1 | CONFIRMED | M | S2 | fe-cli-dbg |
| FE-05 | P1 | CONFIRMED | M | S2 | fe-cli-dbg |
| MIR-11 | P1 | CONFIRMED | S | S2 | mir |
| MIR-14 | P1 | CONFIRMED | S | S2 | mir |
| P0-13 | P1 | CONFIRMED | L | S3 | p0-layout |
| P0-10 | P1 | CONFIRMED | M | S4 | p0-layout |
| DBG-04 | P1 | CONFIRMED | L | S5 | fe-cli-dbg |
| P0-08b | P1 | CONFIRMED | M | S6 | p0-rooting |
| RT-01 | P1 | CONFIRMED | L | S6 | rt |
| RT-02 | P1 | CONFIRMED | S | S6 | rt |
| RT-03 | P1 | CONFIRMED | M | S6 | rt |
| RT-04 | P1 | CONFIRMED | L | S6 | rt |
| RT-05 | P1 | CONFIRMED | S | S6 | rt |
| RT-07 | P1 | CONFIRMED | M | S7 | rt |
| RT-17 | P1 | CONFIRMED | M | S7 | rt |
| RT-18 | P1 | CONFIRMED | S | S7 | rt |
| DBG-05 | P1 | CONFIRMED | XL | S8 | fe-cli-dbg |
| DBG-06 | P1 | CONFIRMED | M | S8 | fe-cli-dbg |
| IP-12 | P1 | CONFIRMED | L | S8 | ip-compile |
| MIR-12 | P1 | CONFIRMED | M | S8 | mir |
| MIR-13 | P1 | CONFIRMED | L | S8 | mir |
| MIR-02 | P1 | CONFIRMED | L | S9 | mir |
| MIR-16 | P1 | CONFIRMED | L | S9 | mir |
| DBG-01 | P1 | CONFIRMED | S | S10 | fe-cli-dbg |
| DBG-02 | P1 | CONFIRMED | M | S10 | fe-cli-dbg |
| RT-12 | P1 | CONFIRMED | L | S10 | rt |
| TY-01 | P1 | CONFIRMED | L | S11 | ty-typedb |
| TY-02 | P1 | CONFIRMED | S | S11 | ty-typedb |
| TY-03 | P1 | CONFIRMED | L | S11 | ty-typedb |
| TY-04 | P1 | CONFIRMED | M | S11 | ty-typedb |
| TY-06 | P1 | CONFIRMED | XL | S11 | ty-typedb |
| TY-07 | P1 | CONFIRMED | L | S11 | ty-typedb |
| TY-22 | P1 | CONFIRMED | M | S11 | ty-flow |
| FE-02 | P1 | CONFIRMED | S | S12 | fe-cli-dbg |
| FE-04 | P1 | CONFIRMED | L | S12 | fe-cli-dbg |
| FE-06 | P1 | CONFIRMED | M | S12 | fe-cli-dbg |
| TY-08 | P1 | CONFIRMED | L | S13 | ty-scope |
| TY-09 | P1 | CONFIRMED | S | S13 | ty-scope |
| TY-10 | P1 | CONFIRMED | M | S13 | ty-scope |
| TY-11 | P1 | CONFIRMED | S | S13 | ty-scope |
| TY-13 | P1 | CONFIRMED | L | S13 | ty-scope |
| TY-14 | P1 | CONFIRMED | S | S13 | ty-scope |
| TY-15 | P1 | CONFIRMED | S | S13 | ty-scope |
| TY-23 | P1 | CONFIRMED | S | S13 | ty-flow |
| TY-24 | P1 | PARTIAL | S | S13 | ty-flow |
| TY-16 | P1 | CONFIRMED | S | S14 | ty-flow |
| TY-17 | P1 | CONFIRMED | M | S14 | ty-flow |
| TY-18 | P1 | CONFIRMED | M | S14 | ty-flow |
| TY-19 | P1 | CONFIRMED | L | S14 | ty-flow |
| HIR-01 | P1 | CONFIRMED | XL | S15 | hir-mono |
| HIR-08 | P1 | CONFIRMED | S | S15 | hir-mono |
| MONO-01 | P1 | CONFIRMED | L | S15 | hir-mono |
| MONO-02 | P1 | CONFIRMED | M | S15 | hir-mono |
| MONO-03 | P1 | CONFIRMED | M | S15 | hir-mono |
| HIR-03 | P1 | CONFIRMED | M | S16 | hir-mono |
| HIR-04 | P1 | CONFIRMED | L | S16 | hir-mono |
| HIR-05 | P1 | CONFIRMED | M | S16 | hir-mono |
| HIR-06 | P1 | CONFIRMED | L | S16 | hir-mono |
| HIR-07 | P1 | CONFIRMED | M | S16 | hir-mono |
| RT-08 | P1 | CONFIRMED | L | S17 | rt |
| TY-25 | P1 | CONFIRMED | S | S17 | ty-ops |
| TY-27 | P1 | CONFIRMED | M | S17 | ty-ops |
| TY-28 | P1 | CONFIRMED | M | S17 | ty-ops |
| TY-29 | P1 | CONFIRMED | XL | S17 | ty-ops |
| TY-31 | P1 | CONFIRMED | M | S17 | ty-ops |
| TY-32 | P1 | CONFIRMED | L | S17 | ty-ops |
| TY-33 | P1 | CONFIRMED | L | S17 | ty-ops |
| RT-13 | P1 | CONFIRMED | L | S18 | rt |
| RT-14 | P1 | CONFIRMED | L | S18 | rt |
| RT-15 | P1 | CONFIRMED | M | S18 | rt |
| IP-01 | P1 | CONFIRMED | S | S19 | ip-compile |
| IP-05 | P1 | CONFIRMED | M | S19 | ip-compile |
| IP-06 | P1 | CONFIRMED | S | S19 | ip-compile |
| IP-07 | P1 | CONFIRMED | M | S19 | ip-compile |
| IP-09 | P1 | CONFIRMED | M | S19 | ip-compile |
| IP-10 | P1 | CONFIRMED | S | S19 | ip-compile |
| IPR-01 | P1 | CONFIRMED | L | S20 | ip-runtime |
| IPR-02 | P1 | CONFIRMED | M | S20 | ip-runtime |
| IPR-04 | P1 | CONFIRMED | M | S20 | ip-runtime |
| IPR-05 | P1 | CONFIRMED | M | S20 | ip-runtime |
| IPR-06 | P1 | CONFIRMED | L | S20 | ip-runtime |
| IPR-07 | P1 | CONFIRMED | S | S20 | ip-runtime |
| IPR-08 | P1 | CONFIRMED | S | S20 | ip-runtime |
| IPR-10 | P1 | CONFIRMED | M | S20 | ip-runtime |
| IPR-12 | P1 | CONFIRMED | M | S20 | ip-runtime |
| IPR-13 | P1 | CONFIRMED | M | S20 | ip-runtime |
| IPR-14 | P1 | CONFIRMED | L | S20 | ip-runtime |
| MIR-03 | P1 | CONFIRMED | M | S21 | mir |
| MIR-04 | P1 | CONFIRMED | L | S21 | mir |
| MIR-05 | P1 | CONFIRMED | M | S21 | mir |
| MIR-06 | P1 | CONFIRMED | L | S21 | mir |
| MIR-07 | P1 | CONFIRMED | M | S21 | mir |
| MIR-08 | P1 | CONFIRMED | M | S21 | mir |
| CLI | P2 | CONFIRMED | S | S2 | fe-cli-dbg |
| FE-07 | P2 | CONFIRMED | M | S2 | fe-cli-dbg |
| MIR-15 | P2 | CONFIRMED | S | S2 | mir |
| P0-14 | P2 | CONFIRMED | S | S2 | p0-layout |
| RT-19 | P2 | CONFIRMED | S | S2 | rt |
| P0-09 | P2 | CONFIRMED | S | S4 | p0-layout |
| P0-08c | P2 | CONFIRMED | M | S6 | p0-rooting |
| RT-10 | P2 | CONFIRMED | M | S7 | rt |
| RT-11 | P2 | CONFIRMED | S | S7 | rt |
| MIR-10 | P2 | PARTIAL | L | S9 | mir |
| RT-16 | P2 | CONFIRMED | M | S10 | rt |
| TY-05 | P2 | CONFIRMED | M | S11 | ty-typedb |
| DBG-03 | P2 | CONFIRMED | S | S12 | fe-cli-dbg |
| TY-12 | P2 | CONFIRMED | S | S13 | ty-scope |
| TY-20 | P2 | CONFIRMED | M | S14 | ty-flow |
| TY-21 | P2 | CONFIRMED | L | S14 | ty-flow |
| HIR-02 | P2 | CONFIRMED | S | S15 | hir-mono |
| HIR-09 | P2 | CONFIRMED | S | S15 | hir-mono |
| TY-26 | P2 | CONFIRMED | S | S17 | ty-ops |
| TY-30 | P2 | CONFIRMED | L | S17 | ty-ops |
| TY-34 | P2 | CONFIRMED | M | S17 | ty-ops |
| IP-02 | P2 | CONFIRMED | S | S19 | ip-compile |
| IP-03 | P2 | CONFIRMED | S | S19 | ip-compile |
| IP-04 | P2 | CONFIRMED | S | S19 | ip-compile |
| IP-08 | P2 | CONFIRMED | S | S19 | ip-compile |
| IP-11 | P2 | PARTIAL | M | S19 | ip-compile |
| IPR-09 | P2 | CONFIRMED | M | S20 | ip-runtime |
| IPR-11 | P2 | CONFIRMED | S | S20 | ip-runtime |
| FE-03 | P3 | CONFIRMED | S | S2 | fe-cli-dbg |
| FE-08 | P3 | PARTIAL | S | S2 | fe-cli-dbg |
| IP-13 | P3 | CONFIRMED | S | S2 | ip-compile |

### 4.1 Findings discovered during the repair

Defects the audit did not have, found while executing this plan. Each was
reproduced against the tree at the commit named, and the reproduction is in the
row — these have no verification data behind them, so the row **is** the
evidence and a stage owner should re-reproduce before fixing.

They are separated from the table above for provenance only. They are scheduled,
they have owning stages, and a stage is not done until its rows are.

**REP-15 and REP-19 are done** (ADR-066, ADR-067). REP-15 was the last P0 and
never got a stage — it arrived after every stage that could own it was written —
and it took the vertical slice the row asked for. REP-19 closed with it, and with
it §3.3's representative program runs **verbatim from the design doc**. **Every
row in this register is now done**: REP-10 closed S25's schedule (ADR-069),
REP-21 followed it (ADR-070), and **REP-24 and REP-25 are new** — found while writing REP-10's
corpus program, and the design doc's own two declaration examples did not parse. REP-22 was
reassessed to P0 after measuring its closure form and closed in the same session. REP-23, which
REP-15 turned up in passing, landed with it — ADR-066's null slot was already its
fix. **The register runs to REP-31 now** — six more rows, found by reading the
design doc's own programs against the front end — and every one of those is done
too.

**REP-16 … REP-20 belong to S25 by subject** and were each found by measuring its
acceptance criterion directly, one at a time, because each was invisible until the
one before it landed: **§3.3's representative program needs all five, and the
stage's `Ordering` note is wrong to say REP-07 + REP-08 + REP-09 are what stand
between it and the compiler.**

So S25's exit list is **nine** rows, not four, and **all nine have landed**:
REP-07, REP-08, REP-17, REP-16, REP-09, REP-18, REP-20, REP-19 and REP-10 — and
**§3.3's representative program compiles and runs verbatim from the design doc**
(`tests/aoc-corpus/s33_representative_program.px` *is* that text, and answers `5`
and `12`). **S25 is closed**, and with it every row this register holds.

The row about `counts.values().count(…)` "already working" was wrong: **neither
half of that line existed** (REP-18), and the claim survived because the program
failed earlier every time it was measured.

**REP-26 … REP-31 are the register's second half**, found at `d8179e1` by a
fresh-eyes sweep that read the design doc's own example programs against the
parser and the inferer, and every one of them is **done**. Three of the six are
the same discovery REP-24, REP-09 and REP-07 were: **the design doc does not
compile.** §4.9's field-reading function, §4.2's shadowing closure and Appendix
D's "first public demo" each fail, and §4.2/§5.7/§6.4 spell a property syntax the
language has never had.

The other three are what the register's own recent rows made reachable. REP-27 —
a line-leading `(` claimed as an argument list, which silently deletes every match
arm after the first — is a trap **ADR-049 measured and consciously left open**,
on the grounds that the workaround was to bind the tuple to a name; REP-10's
tuple patterns and REP-25's `for` bindings took that workaround away, because a
match arm's pattern is how the arm is written. REP-30 is REP-07's own collateral:
making `||` a token took away the accident by which a zero-parameter closure used
to parse. And REP-28 is TY-30's defect at the other member syntax, fixed by
TY-30's own machinery — the constraint channel now has a third capability
discharged by *resolving*.

Two of the six were reproduced independently before being written down, and all
six were re-reproduced against the tree before being fixed, which is what §4.1's
header asks of a stage owner.

| ID | Sev | Effort | Stage | Subsystem | Found | What |
|---|---|---|---|---|---|---|
| REP-01 | P0 | M | S24 | hir-mono | S17 unit 3 (`fb82f79`) | A top-level `fn` in **value position lowers to `Unit`**. `fn double(n: Int) -> Int { n * 2 }` then `let f = double\n f(3)` passes `praxis check` and **aborts the host with SIGBUS**: inference accepts it (a `fn`'s type *is* a `Func`), lowering evaluates the bare name to `Unit`, and `Inst::CallIndirect` reads that Unit's payload as a function pointer. Blocked on **D15**. |
| REP-02 | P2 | S | S23 | rt | S17 unit 2 (`e801e6a`) | `gc_alloc` is generic over its payload and asserts the width at **runtime**, so `gc_alloc(ctx, &scalars::INT, 0)` — an `i32` literal — aborts with "payload size mismatch for descriptor Int" from inside `extern "C"`. §10.4 forbids a non-unwinding panic there. Not reachable from a Praxis program; it is a latent trap for runtime authors, and it cost one debugging cycle. The descriptor knows its payload type, so the signature does not have to be generic. |
| REP-03 | P1 | M | S26 | ty-ops | S17 unit 4 (`b6ab8eb`) | **A `for` over an unannotated parameter unifies the parameter with its own element type.** `capability::iter_item` answers an unresolved receiver with *itself*, so the loop variable and the iterator become one variable and any use of the item pins the iterator: `fn total(r) { var t = 0\n for i in r { t = t + i }\n t }` reports `Y005` "values of type `Int` cannot be iterated" — a legal program rejected. Identically for `Vec`, `BitSet` and `Range`. The fix is for `iter_item` to answer an unresolved receiver with a **fresh** item variable and let the deferred `Iterable { item }` relate the two, which is REP-04 from the other end. Fix both together. |
| REP-04 | P2 | M | S26 | ty-ops | S17 F10 (`e04fcf7`) | **`Iterable`'s `item` is not unified at discharge.** `capability::check` answers the yes/no; a constraint that resolves to a *differently*-itemed iterable is not caught. It is the one place the channel is weaker than its `Capability` shape suggests, and it is REP-03's other end. |
| REP-05 | P1 | S | S26 | hir-mono | S16 (`e918741`) | **A wrong sub-pattern count is silently truncated.** With `enum W { Wrap(Int) }`, `match w { Wrap(a, b) => a }` **compiles and runs**, returning the payload; `b` is lowered (so a mistake inside it still reports) and then dropped. `Y122`/`Y123` cover the two neighbouring mistakes and this one has no code. Truncating is safer than the payload read past the end it replaced, but accepting is not the answer. Needs a `Y12x`; `Y124` is next free (ADR-051). |
| REP-06 | P1 | S | S26 | ty-scope | S13 (`de6b0e2`) | **A nested `struct`/`enum` is silently dropped.** `register_top_level` never declared one and the declaration pass walks only top-level statements, so a `struct` inside a block has no symbol and no type. Declaring one is accepted in silence and *using* it is `N001` "`Inner` is not defined" — pointing at a name declared two lines above. `resolve_top_stmt`/`resolve_block_stmt` is where an `N005`-shaped report goes; `N005` is already "cannot be declared inside another function" for `fn`. |
| REP-07 | P1 | M | S25 | fe-cli-dbg | S17 unit 3 (`fb82f79`) | **There is no `&&` or `||`.** `p.x == 2 && p.y == 2` is a parse error at the first `&`; `praxis-syntax` has a single `AMP` token and no binary production for it. `!` exists. **§3.3's representative program in the design doc uses `&&`** (`if !diagonals && dx != 0 && dy != 0`), so this blocks it. Short-circuit evaluation is the point, so this is a lowering change as well as a grammar one. |
| REP-08 | P1 | M | S25 | fe-cli-dbg | S17 unit 3 (`fb82f79`) | **A tuple has no element syntax.** `p.0` is `P001` "expected name after `.`". A `(Int, Int)` is a legal value, a legal `Map` key and a legal graph state (ADR-060) that **no function can read**. `tests/aoc-corpus/day10_bfs_shortest_distance.px` says so in a comment and hand-encodes its adjacency around it; §6.5's own example needs it. |
| REP-09 | P2 | M | **S25 — done** (`2f9bcbd`, ADR-065) | fe-cli-dbg | S17 unit 2 (`e801e6a`) | **Explicit type arguments on a constructor call have no grammar.** `Counter[(Int, Int)]()` — which §3.3 writes — is a `P002` at the `[`. The element type is inferred from use instead, so `Counter()` works; the design doc's own spelling does not parse. With REP-07 this is the rest of what stands between §3.3 and compiling. **Landed** — and REP-16 turned it into an *ambiguity* rather than a missing form (`Ident [ … ]` is a subscript too), resolved by a closed list of type-constructor names in the parser (ADR-065). |
| REP-10 | P2 | L | **S25 — done** (`ca7385e`, ADR-069) | fe-cli-dbg | S16 (`e918741`) | **Records and tuples have no pattern syntax.** `match p { P { x, y } => x }` is `P001` "expected `=>` in match arm". Their exhaustiveness signature is `Open` in consequence, so a match on one needs a `_`. When the grammar grows one they become `Closed` with a single constructor and `exhaustive.rs`'s matrix already handles that shape — the checker is ready, the parser is not. **Landed** — one production per composite, a record field its own `PATTERN_FIELD` node, and a record pattern's sub-patterns positional over the declared fields (padded, like a variant's payload) so MIR reads slot *i* for sub-pattern *i*. `exhaustive.rs` gained two `Ctor` rows and no new case, exactly as this row predicted. Collateral: a wildcard component is no longer *read* at all, so `Some` reads no payload where `Some(n)` reads one. |
| REP-11 | P2 | S | S23 | fe-cli-dbg | S17 (`f87e6ab`) | **The lexer rejects digit separators that `lower.rs` strips.** `1_000` is a `P002`; `9_223_372_036_854_775_808` lexes as `9` followed by an identifier. `lower.rs` strips `_` from an `IntLit` before parsing it, so **one of the two is dead code**. Either accept them in the lexer or delete the strip. |
| REP-12 | P2 | S | S23 | ty-ops | S15 (`b560e67`) | **`Grid` has no `len`, and a corpus program calls it.** `tests/aoc-corpus/day02_grid_of_char.px` calls `map.len()` on a `Grid[Char]` and gets `Y110` at *lowering*, so `praxis check` is clean and `praxis run` is not. The catalog has `width`/`height`. Either add `Grid.len` (cells, presumably) or fix the corpus program — but no Rust test covers the corpus directory, which is why nothing caught it; **the test is part of the fix**. |
| REP-13 | P3 | S | S23 | ty-typedb | S17 unit 4 (`b6ab8eb`) | **A nullary collection renders with empty brackets.** `TypeDb::render` prints `[...]` whether or not there are arguments, so a `Y001` about a range says "found `Range[]`". Cosmetic; it predates TY-34 (`BitSet[]` has always done it). |
| REP-14 | P2 | M | S26 | ty-scope | S13 (`de6b0e2`) | **A type-declaration cycle registers a fresh variable, silently.** `struct A { b: B }` / `struct B { a: A }` and `struct Node { next: Node }` come out of the declaration pass with a variable where the recursive member should be. ADR-052 puts *supporting* recursive types out of scope (they are equi- or iso-recursive types, a language feature) — this row is the narrower defect that survives either answer: the silence. Blocked on **D17**. |
| REP-16 | P1 | L | **S25 — done** (`0ee29b1`, ADR-064) | fe-cli-dbg | S25 REP-08 (`bb3bc43`) | **There is no subscript syntax.** `m[key]` is a `P001` at the `[` and `counts[key] += 1` is a `P002`; the postfix loop has no `L_BRACK` arm. §4.7 writes `let value = map[key]` and says indexing a missing key **faults** where `.get` returns an `Option` — "the user chooses between explicit absence with `.get` and assertion-like access with indexing" — and §6.2 writes `counts[key] += 1` for a `Counter`, plus `distance[key] min= candidate` and `best[key] max= score`, which are two more assignment operators that do not exist either. **§3.3's representative program uses `counts[point] += 1`**, so this blocks S25's acceptance criterion just as REP-07 and REP-08 did. It is a read form, a compound-assign form, an lvalue in the assignment grammar and a per-collection lowering, so it is L. Needs a decision on how far to go: the read and `+=` are what §3.3 needs; `min=`/`max=` are §6.2's and can be separate. **Landed as a catalog row** under the unspellable names `[]`/`[]=` (ADR-064), so dispatch, the `HasMethod` deferral, bounds and monomorphization are the ones method calls already use. Six collections read, three store; `min=`/`max=` are **not** done and are REP-21. |
| REP-17 | P2 | S | **S25 — done** (`11976cc`) | fe-cli-dbg | S25 REP-08 (`bb3bc43`) | **A trailing comma in an argument list is parsed as an extra argument.** `max(\n  abs(dx),\n  abs(dy),\n)` reports `expected (Int, Int) -> Int, found (Int, Int, ?T) -> ?U` — `parse_arg_list` loops on `eat(COMMA)` and does not check for the closing `)` afterwards, so the trailing comma opens a third argument that parses as nothing. **§3.3's representative program writes exactly that call**, over three lines with a trailing comma. The fix is one `if self.at(R_PAREN) { break }` in the loop; the same shape should be checked for the other comma-separated lists (tuple, `Vec` literal, record literal, param list, match arms). **Landed — and nine of the twelve lists had the hole**, not one: call arguments, `fn` and closure parameters, struct fields, enum variants, an enum payload, collection type arguments and record-literal fields. |
| REP-18 | P1 | M | **S25 — done** (`b495e93`) | ty-ops | S25 REP-09 (`2f9bcbd`) | **A keyed collection cannot be enumerated, and `count` takes no predicate.** §3.3's last line is `counts.values().count(|n| n >= 2)` and **neither half existed**: `values` appears nowhere in the design doc except that line and was in no catalog row, and `count` was defined only at arity zero. The previous session's note that this line "already works" was measured on a program that failed earlier. Landed as four rows — `Counter.keys()`/`values()` and `Map.keys()`/`values()`, each answering a `Vec` so every §6.3 combinator applies — plus a second *arity* of `count`, which the catalog's `(receiver, name, arity)` key has always allowed. The order is **fixed** (by the key's rendered form, RT-16's rule) because a `HashMap`'s is randomized per process, and here it would change a program's answer and not only its printing; `keys()` and `values()` are index-aligned in consequence. The `Map` pair is also the only way to enumerate a `Map` while REP-15 stands. |
| REP-19 | P1 | M | **done** (`ADR-067`) | fe-cli-dbg | S25 REP-18 | **A top-level statement is analyzed and then never executed.** `out(1)\nlet x = 2\nout(x)` passes `praxis check`, and `praxis run` prints **nothing**: `TypedModule.items` holds only `fn` declarations, `lower_module` emits only those, and `run.rs` calls the `main` function. A file with no `fn main` at all is "error: no `main` function to run" after a clean check. **§3.3's representative program is written entirely at top level**, as is §4.2's `let width = grid.width`, so the design doc's own programs are the ones this silences — and the trailing `main()` call every corpus program ends with is decorative for the same reason. The fix is a synthetic entry point: lower the file's top-level statements into a `main` (or into a block that runs before it), which needs a decision about what happens when the file *also* declares `fn main`. **Fixed** (ADR-067) — and §3.2 already required it: "top-level statements are wrapped in a generated entry function". They lower into one generated item named `<entry>`, which is **not an identifier**, so no program can declare or call it. The decision the row asked for is answered: the generated entry wins, and a declared `fn main` is the fallback for a file with no top-level statements — which is every corpus program and every end-to-end test, so nothing had to be rewritten. **§3.3's representative program now runs verbatim from the design doc**, top-level statements and all, which is what S25's acceptance criterion was still short of. |
| REP-20 | P1 | S | **S25 — done** (`b495e93`) | input-parser | S25 REP-18 | **A template literal that begins with a space can never match.** `` `{a:int} -> {b:int}` `` faults `ParseFailed` at the `-` on every input. `walk_template` honours a literal's whitespace policy *before* matching its bytes, and `scan_template` left the leading space run in the text as well, so the same space had to be consumed twice. **§3.3's own template is exactly that shape.** The trailing run had the mirror defect, one degree milder: `1 -> 2` matched and `1->2` did not. Fixed in the scanner — a space/tab run at either *end* of a literal is the policy and leaves the text; a run inside one is untouched, and `\x20` is still the escape for a space that must match exactly. |
| REP-21 | P3 | S | **done** (`8833cb7`, ADR-070) | fe-cli-dbg | S25 REP-16 (`0ee29b1`) | **`min=` and `max=` do not exist.** §6.2 writes `distance[key] min= candidate` and `best[key] max= score`, and says "for `min=` and `max=`, an absent entry accepts the first value" — which no read-modify-write over the subscript rows can express, because a subscript read of an absent `Map` key faults (§4.7). The runtime already has both wrappers, `praxis_map_update_min` and `praxis_map_update_max`, **with no caller**, and `praxis_stdlib::catalog` already reserves the row names `[]min=`/`[]max=` (ADR-064). So what is left is the grammar: `min` and `max` are identifiers, so `min=` is two tokens, and the rule has to be contextual — at an assignment position, an `Ident` spelling `min`/`max` followed by `EQ`. §19's milestone criteria list "`min=` and `max=` map updates work". **Landed exactly as the row describes** — two rows on a `Map[K, V: Int]` receiver (the bound is what the wrappers' `int_payload` needs, and it *pins* an unresolved value type), and the operator is decided in the parser with **adjacency** as the rule: `min = 3` with a space is still two tokens. The pair is an `UPDATE_OP` node so the `=` is not a direct child of the statement, and `PlaceAssignStmt::op()` answers a `PlaceAssignOp` rather than a token — the token accessor is gone, which is what made every call site a compile error. MIR needed no change. |
| REP-22 | P0 | M | **done** (`ADR-068`) | hir-mono | REP-19 (`ADR-067`) | **A `fn` body that reads a top-level binding answers `Unit`, silently.** `let x = 1` / `fn f() { x }` / `out(f())` passes `praxis check` and prints `Unit`. Resolution resolves `x` to the top-level symbol and inference types it, but MIR has no slot for it inside `f` — a `fn` does not capture (§4.9/§4.10: closures do, functions do not), and nothing says so. **Pre-existing** — the binding had no lowering at all before REP-19 — but REP-19 makes it reachable in the design doc's own program shape, where top-level bindings are the norm. Either report it (a `fn` is not a closure, so this is a name mistake in the `N0xx` block) or make top-level bindings real globals, which is a language decision §3.2 does not make. The narrow defect that survives either answer is the **silence**, which is REP-14's shape. **Through a closure it is worse than `Unit`: `let x = 5` / `fn f() { |n| n + x }` / `out(f()(1))` prints a nine-digit number** — capture analysis captures a symbol with no slot, so the environment cell holds whatever the read found. Both forms were measured at `a1c0b76` (before REP-19) and answer the same way there, so REP-19 changed neither; it changed how easily a program reaches them. **P0 for the closure form's silently wrong answer**, which is the same severity bar REP-15's `MinHeap` met. **Fixed: reported as `N007`** (ADR-068). §4.9/§4.10 already drew the line — only §4.10 says "capture" — so this is the line, at the use site, in resolution. The boundary is a **`fn` body** and not a closure body, so a top-level closure and a closure over its own function's locals are both untouched; it is the symbol's **kind** that decides, so another `fn`, a `struct`, an `enum`, a variant and a builtin are all still reachable. The reference is still recorded, so there is no cascade. Making top-level bindings *globals* was the alternative and is a language feature §3.2 does not ask for. |
| REP-23 | P1 | S | **done** (`ADR-066` decision 5) | mir-codegen | REP-15 (`ADR-066`) | **A fused `enumerate()`'s pair is an empty tuple.** `v.enumerate().collect()` over `[10, 20]` answers `[(), ()]` — **both elements dropped**. MIR emits `AllocKind::Tuple { ty: MirType::Opaque, elements: [idx, item] }` for the fused pipelines (their item types arrive with MIR-05, S21), `tuple_schema_for` degrades `Opaque` to a **zero-element** schema, and `praxis_alloc_tuple` sizes the payload from the schema — so `praxis_tuple_set` writes into a zero-length `items` and silently drops both. The type gap is MIR-05's; the *dropping* is not, and it is a silently wrong answer out of a documented §6.3 combinator. REP-15 made the fix small and it landed with the row: a schema slot may be **null** (ADR-066 decision 5), so `tuple_schema_for` takes the arity from `elements.len()` when the type gives none and leaves every slot unknown; the values' own descriptors answer. `an_opaque_tuple_type_yields_an_empty_schema` was a §8.2 bug-pinning test — it asserted the empty schema — and is rewritten. The static types are still MIR-05's (S21); the dropping is nobody's now. |
| REP-24 | P2 | S | **done** (`33b386f`) | fe-cli-dbg | S25 REP-10 (`ca7385e`) | **A declaration's members must be comma-separated, and the design doc writes them on separate lines.** §4.5's own `struct Point {\n    x: Int\n    y: Int\n}` is `P001` "expected `}` to end struct fields" and §4.6's `enum Tile { Empty\n Wall\n Number(Int)\n Portal(Text) }` is the same at its own brace: both lists loop on `eat(COMMA)` alone. So **neither declaration example in the design doc parses**, which is why every declaration in the corpus and in the suite is written on one line — the defect shaped the tests rather than being caught by them. Match arms have taken a comma **or** a line break since FE-04 (D8, ADR-049); this is the same rule at the same kind of brace. **Landed**: one shared `member_separator`, and a trailing comma still closes the list either way (REP-17). |
| REP-25 | P2 | M | **done** (`0066a6f`) | fe-cli-dbg | S25 REP-10 (`ca7385e`) | **A `for` binding is one name, so `for (k, v) in m` cannot be written.** The header runs `expect(Ident)`, so a `Map`'s pair has to be named and then read with `kv.0`/`kv.1`. ADR-066 decision 3 deferred this to REP-10 — "destructuring in binding position is a *pattern*, and patterns for records and tuples are REP-10 — the same grammar, and no reason for two" — so once REP-10 landed what was left was the **binding position**, not the grammar. **Landed**: `TypedExpr::For.binding` is a `TypedPattern`, a bare name is a `Bind` that lowers exactly as before, and MIR's `bind_components` is `emit_pattern_test` with the testing half removed. A binding has no second arm, so a refutable pattern is the new `Y125`. |
| REP-15 | P0 | L | **done** (`ADR-066`) | mir-codegen | S26 REP-03 (`586a149`) | **Six of the nine iterable collections have no `for` lowering: iterating one segfaults or answers with garbage.** `for x in s` over a `Set`, `for i in b` over a `BitSet` and `for kv in c` over a `Counter` each pass `praxis check` and **kill the process**; `for kv in m` over a `Map` likewise; and a `MinHeap` is worse — `for x in h` over `[3, 1, 2]` sums to `4349199564`, a **silently wrong answer** out of a program that reports nothing. MIR's `get_symbol_for` has arms for `Vec`, `Deque` and `Range` and a `_ => VecGet` default, so a `Set` object's payload is read as a `Vec`'s; `len_symbol_for` has the same default. It is not a missing match arm: the runtime has **no indexed accessor** for `Set`, `BitSet`, `Map`, `Counter`, `Grid` or the heaps to select — `MapGet`/`CounterGet` are keyed lookups and `GridGet` takes `(x, y)`. So it needs an iteration protocol (an nth-member symbol per collection, or a cursor) plus a `for` lowering that uses it, which is D6's Range slice again at six times the width. `capability::iter_item` has claimed all nine are iterable since M8; nothing ever lowered six of them, and no test ran a `for` over one. **Needs a decision** on the protocol *and* on whether `for (k, v) in m` destructures, since `for kv in m` is the only spelling today (REP-10's tuple patterns are the other half of that). **Fixed: a `for` iterates a snapshot** (ADR-066). A collection that cannot index itself materializes its members into a `Vec` once before the header and the loop walks that with the `Vec` accessor pair; `Map`/`Counter` snapshot **twice** through REP-18's index-aligned `keys()`/`values()` and pair in MIR, so they need no new wrapper. Four new wrappers (`praxis_set_items`, `praxis_bitset_items`, `praxis_{min,max}_heap_items`); `Grid` reuses `praxis_grid_cells`. `IterPlan` is exhaustive over `CollectionCtor` — the `_ => VecGet` default *was* the defect. The destructuring half is answered **no**: the binding stays one name and `kv.0`/`kv.1` read the pair, so `for (k, v) in m` is REP-10's pattern grammar. Collateral: an unresolved tuple-schema slot is **null** and the value's own descriptor answers for it, or `let m = Map()` plus a `for` that never opens the pair fails to compile. |
| REP-26 | P0 | S | **done** (`ac1f437`) | hir-mono | design-doc sweep (`d8179e1`) | **A record literal whose head is not a `struct` lowers to `Unit`, silently.** `let x = 1` / `let p = x { a: 1 }` / `out(p)` passes `praxis check` clean and prints `Unit`; `out(p + 1)` prints **33634418689**, a raw pointer added to one. `infer_record_lit` reads the head symbol's *type* and never asks what the symbol is, so a head that is not a record falls out of the `TypeData::Record` match through `return struct_ty` and the literal comes out typed as whatever the head was, with no lowering behind it. That is **REP-01's shape** — a program `praxis check` accepts whose value has no representation — and REP-01 took a SIGBUS for the same reason. It must be a diagnostic at **analysis** time so `check` catches it (REP-12), and ADR-050 is why the check goes in inference rather than the parser: a record literal is legal wherever a brace cannot be a block, so the parser cannot know what the head names. **Landed as `N008`**, decided on the symbol's **kind** — REP-22's rule at another door, and the only one that works in both directions: a parameter's type is a variable, so `fn g(q) { q { a: 1 } }` would look like a success under a type test, and an `enum` is a genuine type, so a message claiming "not a type" would be a lie about it. Every non-`struct` kind reports and the message names the kind. A head that resolves to nothing is still `N001` alone. ADR-051 amended; `N009` is next. |
| REP-27 | P1 | M | **done** (`bd3c34e`, ADR-049 amended) | fe-cli-dbg | design-doc sweep (`d8179e1`) | **A `(` at the start of a line is read as the previous expression's argument list, and a `match` silently loses every arm after the first.** `parse_postfix` opens a `CALL_EXPR` on any `L_PAREN` reachable from `peek()`, and `peek()` skips trivia — a newline **is** trivia. So a newline-separated arm list whose next arm begins `(a, b) =>` has that arm eaten as the previous arm body's argument list, the arm loop then finds no pattern start, and the second arm and every arm after it leave the tree. Two more doors do the same: `parse_name_or_call`'s primary call form, and the field-vs-method lookahead one token past a `.` (`p.x\n(a, b)` was a method call). **ADR-049 saw this exact shape and left it open** — its D8 discussion rejects "terminate before a line-leading `(`" because the workaround is to bind the tuple to a name — and the workaround stopped existing when REP-10 gave `match` arms tuple patterns and REP-25 gave `for` bindings the same grammar: a match arm's pattern *is* how the arm is written. **Landed** as D8's own rule at a door D8 did not visit: a `(` asked to **continue** an expression does not cross a line break. The Pratt operator loop is untouched (`1 +\n2` and a `.method()` chain still work), and a `(` that *opens* an expression is untouched. `[` is deliberately exempt — no statement and no pattern begins with `[` (REP-16) — and that asymmetry is the decision. The cost ADR-049 named is now an assertion: `f\n(1)` is two expressions. |
| REP-28 | P1 | M | **done** (`5b9bfd9`, ADR-057 extended) | ty-ops | design-doc sweep (`d8179e1`) | **A field access never constrains its receiver, so §4.9's own example does not compile.** `struct P { x: Int, y: Int }` / `fn dist(a) -> Int { a.x + a.y }` / `out(dist(P { x: 1, y: 2 }))` passes `praxis check` clean and then fails under `praxis run` with `Y112` "no field `x` on this type" — twice, inside a function whose only call passes exactly the record that has both fields. `infer_field_get` answers an unresolved receiver with a **fresh variable** and records no constraint, so the parameter is generalized with no requirement to re-ask at the call site. That is TY-29 in its original form, and it is **TY-30's defect at the other member syntax**. **Landed** through the channel — the progress doc's rule that calling a predicate directly is TY-29 by another name — as `Capability::HasField { name, ty }`, discharged by `resolve_deferred_field`, which asks the record the program pinned what the field holds and **unifies**. It is the third capability discharged by *resolving* rather than by answering yes/no, joining `HasMethod` (ADR-057 D5) and `Iterable` (ADR-062 D4). The receiver and the field type are `pin_to_level`'d for D5's reason — `lower_field_get` reads one record *definition* for the field's index, so one read site carries one record type, and two call sites at two records are a `Y001` disagreement about the signature. ADR-062's asymmetry has no counterpart here and that is stated rather than copied: a field read names one record and there are no per-ctor runtime symbols to get right. **Corrected the same day, after review:** the first landing kept D5's division of reports — the failure left to lowering — and that made the requirement inert. `infer_field_get` deferred only a *variable* receiver, so `Capability::HasField`'s rejection arm was unreachable dead code and `capability::check` was never called with it; `resolve_deferred_field` returned silently when the resolved receiver had no such field; and lowering cannot report the deferred case at all, because by then the receiver is still a variable. So all of `let n = 1` / `out(n.x)`, `fn dist(a) { a.x }` / `out(dist(3))` and `v.map(\|a\| a.x)` stayed check-clean and run-broken, and **§4.9's own fence still failed**: `praxis check` rc=0, `praxis run` four `Y112`s, byte-identical to the pre-fix binary. The report is now inference's from both doors — a concrete receiver decided in `require_cap_as`, a deferred one reported by `resolve_deferred_field` through `report_cap_failure`, as `Iterable` already did — and it names the type (`no field \`x\` on type \`Int\``), which lowering's emitter could not. What lowering keeps is the *opposite* case and it is not a failure: a receiver still a variable at lowering is one no call site pinned, and it is accepted, which is what makes §4.9's uncalled `manhattan` compile — the tolerance an uncalled `fn f(a) { a + 1 }` always had. The gate is `crates/praxis-cli/tests/design_doc.rs`, which **extracts §4.9's fence from `praxis_technical_design.md` at test time** and drives the real binary through `check` and `run`; the first landing was gated on a paraphrase with a call site the fence does not have, which is how a four-`Y112` fence was signed off as a fixed two-`Y112` one. |
| REP-29 | P1 | M | **done** (`67642c4`) | fe-cli-dbg | design-doc sweep (`d8179e1`) | **A closure parameter is a bare name, not a pattern, so Appendix D's "first public demo" program does not parse.** `parse_closure`'s parameter loop runs `expect_binder`, so `.map(\|(a, b)\| abs(a - b))` is `expected closure parameter name` at the `(` plus a cascade. REP-25 did exactly this job for the `for` binding one commit earlier and gave the reason ADR-066 decision 3 had already written: destructuring in binding position **is** a pattern and there is no reason for two grammars. This is the third and last binding position. **Landed**: `parse_pattern` in place of `expect_binder` is the whole grammar change, and `Param::name` answers *through* the pattern when the pattern is a name — so `\|x\|` and `\|_\|` take the identical path they always did at every layer, and only a destructuring parameter is new anywhere. A destructuring one needs a slot, because a closure still takes one value per parameter: resolution mints a slot symbol at the pattern's range (never bound into a scope, so no source name reaches it) and lowering wraps the body in a **one-arm `match` on that slot**. That is deliberate and is why **no MIR change was needed** — a record and a tuple have one constructor each (ADR-069), so MIR emits the component reads with no tag to compare and no arm to fall through to. REP-25's `Y125` carries over unchanged: a parameter has no second arm, so `\|Some(n)\| n` is reported. ADR-051 amended to record that REP-29 spends no code. |
| REP-30 | P1 | S | **done** (`ded26de`) | fe-cli-dbg | design-doc sweep (`d8179e1`) | **A zero-parameter closure `\|\| expr` does not parse, so §4.2's shadowing example is a `P001`.** `let f = \|\| 5` is `error[P001]: expected an expression` at the `\|\|` plus a cascading `P002`; §4.2 writes `let show_old = \|\| out(a)`. Only a bare `PIPE` reaches `parse_closure`, and the comment above that arm asserted the lexer's max-munch keeps the two apart — **it is the max-munch that creates the collision**: before REP-07 there was no `\|\|` token and a zero-parameter closure lexed as two pipes and parsed by accident; REP-07 made it `PIPE2` at bp(1, 2) and took the accident away, leaving the comment asserting the opposite. **Landed** as a max-munch question decided by **position**, which is REP-21's answer for `min=` (a contextual operator exactly where a name cannot be one) and REP-09's for `[`: `parse_prefix` is only called where an expression must *begin*, and a binary operator has no left operand there, so a `\|\|` reaching it is the empty parameter list and nothing else. The precedence table is untouched and the infix loop still reads every `\|\|` between two operands — `a \|\| \|\| 5` is both readings in one line. One `PIPE2` token stands for both pipes rather than being split: the tree round-trips the source and the node kind carries the meaning. The gate measures short-circuiting with a side effect the skipped operand would leave. |
| REP-31 | P2 | S | **done** (`185fe43`, ADR-077) | fe-cli-dbg | design-doc sweep (`d8179e1`) | **The design doc writes the zero-argument accessors as property reads in three places and as calls everywhere else.** §4.2's first `let` example is `let width = grid.width`; §6.4's Grid API opens with `grid.width`/`grid.height` and then writes `grid.get(x, y)`, `grid.positions()`, `grid.cells()`; §5.7's example rows are `Vec[T].push(T) -> Unit` and `Vec[T].len -> Int`. Measured: only the call form exists — `len`, `width` and `height` are catalog rows of arity zero, every test and corpus program calls them, and a bare `v.len` reaches `lower_field_get`, finds no record and is `Y112`. **Decided: they are always calls, and the doc is corrected** (ADR-077). Three reasons in binding order. A bare `.name` already means a field read, which lowers to a slot *index* against the record's definition rather than a catalog call, so one syntax meaning either would let a new `struct` field silently change what an existing expression does. A receiver whose type is not yet known cannot tell them apart — REP-28 put a field read on the constraint channel, and a property form would emit a requirement with **two** possible discharges and nothing at the read site to choose between them. And §5.7 says the catalog *is* the dispatch table, which the language server also reads for completion. No grammar changed; four doc lines did, §5.7 states the rule inline, and the gate pins both directions including a record field named `len`. |
| REP-32 | P0 | S | **done** (ADR-049 D7 amended) | hir-mono | reviewer, REP-29 review | **A wildcard parameter is dropped from the lowered parameter list, so every parameter after it takes the wrong argument.** `let f = \|_, b\| b + 1` / `out(f(9, 5))` prints **10**; it must print `6`, because `b` is the second argument and it was reading the first. `fn g(_, b) { b + 1 }` / `out(g(9, 5))` dies with a **Cranelift verifier error** — the lowered body has one slot and the signature has two, which is the same defect where the backend can still see it. `praxis check` is clean for both. Mechanism: `Param::name()` answers `None` for `_` (ADR-049 D7 gives it no name), `Resolver::bind_param` returned early without minting anything, `lower_param` therefore found no declaration and answered `None`, and its two callers are `filter_map`s — so the parameter left the slot list while `infer_param` kept it in the *type*. **Pre-existing**, reproducing identically at `d8179e1`: REP-29 rewrote this path and its gates check parse-tree counts for `\|_\|`, which is why nothing on that branch noticed. Registered separately rather than folded into REP-29 because the `fn` half is not a closure-grammar change and the two are one defect. **Landed**: a wildcard parameter owns an anonymous slot minted at the `_`'s own range and never bound into a scope, exactly as REP-29's destructuring parameter does — `Param::wildcard()` answers for both spellings, the `fn` form's bare `UNDERSCORE` token and the closure form's whole `PatternKind::Wildcard`. A `_` *inside* a pattern is untouched: the enclosing pattern owns that argument. D7's "introduces nothing" is about **names**, not slots, and ADR-049 says so now; `a_wildcard_binder_is_legal_and_declares_nothing` asserted the table had no symbol named `_`, which is a claim about the table, and it now asserts the property D7 actually states — nothing may *read* a `_`. The gate is `a_wildcard_parameter_keeps_its_slot_so_later_parameters_do_not_shift` in `jit.rs`, and every assertion in it is a **value**: a shifted list has to come out as the wrong number rather than as a crash. |
| REP-33 | P2 | M | **registered, not fixed** (deferred to S18/§6.3) | ty-ops | reviewer, REP-29 review | **Appendix D's "first public demo" now parses and type-checks and still does not run: eight `Y110`s, after a clean `praxis check`.** The fence extracted verbatim (`praxis_technical_design.md`, Appendix D) is `praxis check` rc=0 — on `main` the same file is `P001`+`P002`, so REP-29 and REP-30 genuinely fixed the parse — and `praxis run --input day1.txt` rc=1 with `no method \`sorted\` on this type taking 0 argument(s)` twice, then cascading `zip`, `map`, `sum`, `frequencies`, `map`, `sum`. `sorted` and `frequencies` are absent from `crates/praxis-stdlib/src/builtins.rs`. **Not a regression and not a repair:** §6.3 already states that the five barrier combinators `sorted`/`unique`/`frequencies`/`chunks`/`windows` "need the whole sequence and require new runtime sort/dedup helpers; they remain deferred", and the stdlib crate is S18's. Implementing them is a language feature. **What is recorded here is the second half:** a clean `praxis check` followed by a `Y110` at run is the same `check`-does-not-run-lowering gap the reviewers keep hitting — `v.nosuchmethod()` on a concrete `Vec[Int]` is rc=0 under `check` on `main` and on this branch. REP-28's correction closed that gap for `Y112` by requiring `HasField` of every receiver and reporting from inference; `Y110`'s half is still open, and closing it is the same move at `HasMethod` — `resolve_deferred_method` deliberately leaves a missing row to lowering. So "REP-29 done" means Appendix D **parses and checks**, not that the first demo runs; the gate that pins the parse is `every_praxis_fence_in_the_design_doc_parses`. |
| REP-34 | P2 | S | **done** (doc-only; grammar was right) | fe-cli-dbg | reviewer, design-doc fence sweep | **Four `praxis` fences in the design doc do not parse, all of them §7.5 parser-constructor calls with a labelled argument.** `chars(one_of("^v<>"), skip: whitespace)` (L877), `sections(block(ranges: lines(...)))` (L902), `choice(Number: \`...\`, ...)` (L943) and `scan(choice(Multiply: \`...\`, ...))` (L956), each `error[P001]: expected \`)\`, found unexpected token` at the `:`. Identical on `main`, so no regression. **Established which side is wrong: the doc's.** A labelled argument *is* implemented — `parse_parser_call` emits `PARSER_NAMED_ARG` and `parser_lower` consumes it for `sections`, `block`, `choice` and `skip:` — but only inside the **parser-expression sublanguage**, which §7.1 says is entered by `read` or by `parse(text, ...)` and nowhere else. Written bare, those four fences are ordinary call expressions, where `skip:` is a syntax the language does not have. Prefixed with `read`, all four parse and check clean today; no grammar change is needed and `crates/praxis-parser/*` is untouched. **Landed as a doc correction** (REP-31's shape): §7.5's first four fences already wrote `read` and the other eight did not — three of those parsed only by coincidence, as calls to undefined names — so all of §7.5 says `read` now and the section states why. The gate is `every_praxis_fence_in_the_design_doc_parses` in `crates/praxis-cli/tests/design_doc.rs`, a **sweep**: it extracts all 57 ```praxis fences at test time and fails on any `P0xx`, so a fence added later is covered without anyone remembering to. Parse only — a fence is a fragment and its `N0xx`/`Y0xx` are expected; `P0xx` means the text is not the language whatever surrounded it. |

## 5. Stages

Each stage is a landable unit with its own green bar. A stage is done when its
exit criteria pass **and** `just ci` is clean — never one without the other.

### S1 — Runtime identity registry

*1 findings · weight 3*

**Goal**

Make built-in TypeIds globally unique and generated from one exhaustive
registry, so every id-based guard downstream can mean something.

| Finding | Sev | Effort |
|---|---|---|
| P0-01 | P0 | M |

**Ordering**

HARD BARRIER. Every `descriptor().id != X.id` guard proposed by RT-09, RT-10,
RT-11, P0-12, DBG-01, DBG-02 and P0-11 is a verified no-op while FLOAT
(scalars.rs:242) and TEXT (text.rs:132) are both TypeId(5). Landing any of them
first produces green tests that prove nothing — false assurance is worse than
the open bug.

**Exit criteria**

Un-ignore and pass descriptor::tests::builtin_type_ids_are_globally_unique
(crates/praxis-runtime/src/descriptor.rs:219). New compile-time assert that
BUILTINS[i].id.0 == i must exist and TypeId.0 must be private outside
praxis-runtime.

### S2 — Independent hardening (runs parallel with every other stage)

*13 findings · weight 19 · parallelizable within*

**Goal**

Land the self-contained fixes that have zero cross-stage coupling, to shrink
the critical path and get the shared identifier predicate and symbol table in
place early.

| Finding | Sev | Effort |
|---|---|---|
| P0-05 | P0 | S |
| FE-01 | P1 | M |
| FE-05 | P1 | M |
| MIR-11 | P1 | S |
| MIR-14 | P1 | S |
| CLI | P2 | S |
| FE-07 | P2 | M |
| MIR-15 | P2 | S |
| P0-14 | P2 | S |
| RT-19 | P2 | S |
| FE-03 | P3 | S |
| FE-08 | P3 | S |
| IP-13 | P3 | S |

**Ordering**

No stage depends on this one except through FE-01's shared
is_ident_start/is_ident_continue (consumed by IP-04 in S19 and DBG-03 in S12)
and MIR-14's single symbol table (required by MIR-09 in S9 and P0-12 in S10
before either adds a new runtime symbol). Everything else here is terminal.

**Exit criteria**

Un-ignore and pass:
regression_unicode_identifier_may_start_with_a_unicode_scalar (lex.rs:602),
regression_in_is_classified_consistently_with_the_keyword_table (kind.rs:457),
regression_postfix_forms_may_be_interleaved (parse.rs:1553),
regression_formatting_does_not_delete_comments (fmt.rs:269),
for_continue_targets_the_increment_block_not_the_header (build.rs:4000),
missing_explicit_input_file_is_a_usage_error (cli/tests/run.rs:67),
float_sign_of_zero_is_zero (abi.rs:3009). NEW tests required — no gating
regression exists for P0-05 (FileView across a reallocating intern), P0-14
(out-of-range RawSyntaxKind), MIR-14 (a MIR CallTarget::Runtime name absent
from module.rs registration must be a hard JitError, not a dlsym success),
MIR-15 (non-I64 pointer target rejected at Jit::new), IP-13 (doc-only), FE-08
(widened proptest generators).

### S3 — ABI manifest, MIR representation, fault-path values

*5 findings · weight 40*

**Goal**

One declarative ABI/effect manifest; remove raw non-pointer words from
GC-rootable slots; make every fault exit return a valid Unit; replace Type(0)
with MirType::Known|Opaque; lower IntBinOp natively.

| Finding | Sev | Effort |
|---|---|---|
| P0-02 | P0 | XL |
| P0-03 | P0 | M |
| P0-04 | P0 | S |
| P0-08 | P0 | L |
| P0-13 | P1 | L |

**Ordering**

P0-13 must precede every stage that adds a runtime symbol (P0-12's
praxis_struct_cmp in S10, MIR-09's praxis_raise_empty_collection in S9, P0-08's
two non-allocating raise symbols here), or each is added to five places again.
P0-08 is co-located with P0-04 because the two interact: the arithmetic
wrappers already return unit_sentinel on fault (abi.rs:566-570) and codegen
IntLoads it at lower.rs:877 before check_fault at :880; once P0-04 makes
function fault epilogues also return a real Unit, a caller that int_loads a
faulted return gets a silently wrong value instead of a segfault. P0-02 lands
the representation only — the ~35 build.rs sites with no available type are
marked Opaque explicitly, and the verifier's no-Opaque-in-descriptor-position
rule stays OFF until S15.

**Exit criteria**

Un-ignore and pass: closure_capture_indices_never_flow_through_gc_locals
(build.rs:3833), fault_epilogue_returns_the_valid_unit_sentinel
(adversarial_audit.rs:127),
call_result_locals_retain_their_inferred_static_types (build.rs:3929),
pipeline_runtime_call_destinations_retain_vec_and_unit_types (build.rs:3960).
NEW tests required for P0-13 (a symbol absent from the manifest fails to
compile rather than falling back to arity-derived signatures; evaluate.rs
null_sentinel() deleted) and for P0-08 (no AllocInt/IntLoad pair emitted for
Int arithmetic; i64::MIN/-1 div and rem match abi.rs:594-597 and :619-622
exactly). NOTE: enumerate_tuple_allocation_carries_a_real_two_element_type
(build.rs:4047) and zip_tuple_allocation_carries_a_real_two_element_type
(build.rs:4079) are NOT exit criteria here — they need MIR-05's real tuple type
(S21).

### S4 — Object layout and heap provenance (single commit)

*2 findings · weight 4*

**Goal**

Repack GcHeader exactly once: authoritative payload_offset plus a HeapId, with
sweep poisoning the reclaimed header.

| Finding | Sev | Effort |
|---|---|---|
| P0-10 | P1 | M |
| P0-09 | P2 | S |

**Ordering**

HARD BARRIER before S6. P0-09 and P0-10 both add a field to the same #[repr(C)]
GcHeader whose size is baked into JIT-emitted code at lower.rs:1097; repacking
twice means two ABI churns and two chances to desynchronize codegen from the
runtime. Sweep poisoning (descriptor=null, heap_id=0) is the precondition for
RT-01's free-list: reusing swept storage without poisoning upgrades a stale
GcRef from pointing at dead-but-typed memory to pointing at a live object of a
different type.

**Exit criteria**

Un-ignore and pass overaligned_payload_accessor_matches_initialized_address
(heap.rs:595) and foreign_heap_root_cannot_delay_reclamation (heap.rs:619).
GcHeader::payload_offset must be the single layout authority consumed by
heap.rs alloc_raw, gc.rs payload(), and codegen lower.rs:1097 (which today
hardcodes size_of::<GcHeader>()).

### S5 — Root-set completeness and native RAII roots (single commit; hard barrier)

*3 findings · weight 36*

**Goal**

One composite RuntimeRoots covering shadow frames, input_source,
parse_detail.partial, crash snapshot and a new native RAII scope; delete the
null-shadow-frame early return.

| Finding | Sev | Effort |
|---|---|---|
| P0-06 | P0 | L |
| P0-07 | P0 | XL |
| DBG-04 | P1 | L |

**Ordering**

HARD BARRIER before S6 and before IPR-14 in S20. Verified at abi.rs:101-114:
maybe_collect returns early when ctx.roots is null, so during host-driven
allocation and throughout the parser interpreter nothing is currently collected
at all. Deleting that guard is the trigger — if it lands before P0-07's native
arm exists, every unrooted Vec<GcRef> intermediate in parser.rs and in the grid
helpers becomes immediately collectable, converting a growth bug into
use-after-free. P0-06 and P0-07 must therefore be one commit, not siblings in a
stage. DBG-04 is the debugger instance of the same defect (evaluate.rs:257-260
builds a RootScope attached to nothing) and consumes the same field.

**Exit criteria**

Un-ignore and pass automatic_gc_roots_the_ambient_input_buffer (abi.rs:3671),
automatic_gc_roots_parse_failure_partial_values (abi.rs:3691),
automatic_gc_roots_runtime_owned_crash_snapshots (abi.rs:3713),
nested_allocating_helpers_root_intermediate_results (abi.rs:3751). NEW test
required for DBG-04: a `p` expression's argument GcRefs must survive a
collection triggered inside the synthetic function. Heap::collect/maybe_collect
must no longer accept an arbitrary &dyn RootSet. RUNTIME_ABI_VERSION
(abi.rs:48) and COMPILER_EXPECTED_ABI_VERSION (abi.rs:70) bump exactly once
here for the native_roots field.

### S6 — Allocation pacing, effect metadata, heap lifecycle

*7 findings · weight 27*

**Goal**

Pace every allocation at the Heap::alloc boundary, derive allocation effects
from one table, add Drop/reset/immortal-reseat correctness, and make swept
storage reusable.

| Finding | Sev | Effort |
|---|---|---|
| P0-08b | P1 | M |
| RT-01 | P1 | L |
| RT-02 | P1 | S |
| RT-03 | P1 | M |
| RT-04 | P1 | L |
| RT-05 | P1 | S |
| P0-08c | P2 | M |

**Ordering**

P0-08b is the audit's own declared ordering hazard, and it is confirmed: adding
maybe_collect to 22 wrappers while
input_source/parse_detail/crash_snapshot/native intermediates are unrooted
collects live objects. It must follow S5, not accompany it. RT-01 must follow
S4's poisoning for the reason given there. RT-02's Drop makes any host holding
a GcRef past Runtime drop a visible UAF — audit take_crash_snapshot consumers
in praxis-cli and praxis-debugger in the same change. RT-03 must keep
alloc_immortal restricted to Immortals::new or RT-02's Copy-payload drop-order
argument lapses.

**Exit criteria**

Un-ignore and pass checked_int_add_is_an_automatic_gc_safepoint (abi.rs:3636),
dropping_heap_finalizes_live_owned_payloads (heap.rs:571),
bool_and_unit_abi_allocations_reuse_runtime_singletons (abi.rs:2970),
reset_restores_collection_pacing (heap.rs:638),
repeated_collection_reuses_dead_object_storage (heap.rs:653). NEW tests
required for RT-05 (Heap::reset must not strand Runtime.immortals;
Runtime::heap_mut deleted) and P0-08c (a build-time assert that the effect
table covers every symbol registered in symbols.rs, and that
MethodEntry.allocates is deleted rather than merely corrected).

### S7 — Descriptor totality, typed collection construction, fault representation

*8 findings · weight 23*

**Goal**

Make descriptor_for_type exhaustive and fallible, make element descriptors
non-nullable and validated on mutation, and remove the redundant Fault
pending/kind state.

| Finding | Sev | Effort |
|---|---|---|
| P0-11 | P0 | L |
| RT-06 | P0 | M |
| RT-09 | P0 | S |
| RT-07 | P1 | M |
| RT-17 | P1 | M |
| RT-18 | P1 | S |
| RT-10 | P2 | M |
| RT-11 | P2 | S |

**Ordering**

P0-11 must precede RT-10 and RT-11: tightening collection equality to compare
element descriptors will surface P0-11's currently-mislabelled INT descriptors
as a cascade of new failures rather than as the intended assertions. P0-11 also
FLIPS currently-passing tests that assert adopt-on-first-push Vec[Int] tagging
— budget to rewrite them, not just unignore. RT-09/RT-10/RT-11 are gated on S1
for the id-guard reason. RT-17 must precede RT-18, which needs a real FaultKind
to raise. The one JUDGEMENT call is what the JIT does when descriptor_for_type
returns Err: fail the compile with a diagnostic (correct) or fall back
(reintroduces the bug).

**Exit criteria**

Un-ignore and pass
empty_vec_float_has_the_float_element_descriptor_before_any_push
(adversarial_audit.rs:298),
tuple_schema_uses_the_unit_descriptor_for_unit_elements (:493),
tuple_schema_uses_the_enum_descriptor_for_enum_elements (:509),
nested_record_inequality_dispatches_to_the_record_descriptor (:271),
grid_positions_vec_uses_the_point_tuple_descriptor (:524),
grid_text_row_preserves_the_grid_cell_descriptor (:543),
grid_cell_vectors_preserve_the_grid_element_descriptor (abi.rs:3469),
grid_position_vectors_use_the_point_tuple_descriptor (abi.rs:3515),
constructed_grid_cells_satisfy_the_declared_element_descriptor (abi.rs:3496),
vec_push_rejects_a_value_with_the_wrong_descriptor (abi.rs:3433),
dynamic_keys_with_different_descriptors_are_never_equal (dynamic_key.rs:228),
empty_vectors_with_different_element_types_are_not_equal (collections.rs:330),
tuple_equality_uses_shape_not_schema_allocation_identity (tuples.rs:199),
setting_none_cannot_create_a_pending_fault (context.rs:765),
alloc_char_rejects_values_that_only_become_valid_after_truncation
(abi.rs:3453). NEW tests required for RT-06 (an out-of-range or non-boundary
Text slice must be unconstructible, not clamped) and RT-07
(negative/overflowing Grid and BitSet extents fault instead of panicking or
allocating absurdly).

### S8 — Generation arena for JIT and plan metadata

*5 findings · weight 42*

**Goal**

Replace every Box::leak and process-global OnceLock cache with one arena owned
by the JIT generation, keyed so a bare u32 def id cannot name a schema.

| Finding | Sev | Effort |
|---|---|---|
| DBG-05 | P1 | XL |
| DBG-06 | P1 | M |
| IP-12 | P1 | L |
| MIR-12 | P1 | M |
| MIR-13 | P1 | L |

**Ordering**

Must follow S6 because reclamation ordering is the whole difficulty: the
runtime holds raw *const RecordSchema and *const TupleSchema inside live GcRef
payloads, so the arena must be dropped after heap teardown. Landing arena
reclamation before RT-02's Heap Drop ordering is settled produces dangling
schema pointers during finalization. Can run in parallel with S9 and S10.

**Exit criteria**

Un-ignore and pass
record_schema_cache_is_scoped_by_type_database_not_bare_def_id
(adversarial_audit.rs:315). NEW tests required: repeated debugger `p` / reload
must not grow leaked metadata without bound (DBG-05, MIR-13); PLAN_SLAB
registration must be bounded and its u32 narrowing checked (IP-12);
leak_static_str must no longer exist.

### S9 — MIR root exactness, debug/root split, verifier

*5 findings · weight 35*

**Goal**

Separate the debugger-visible set from the GC root set, then make roots exact
and add a MIR verifier; fault empty element-returning sinks.

| Finding | Sev | Effort |
|---|---|---|
| MIR-01 | P0 | L |
| MIR-09 | P0 | M |
| MIR-02 | P1 | L |
| MIR-16 | P1 | L |
| MIR-10 | P2 | L |

**Ordering**

HARD internal ordering: MIR-16 before MIR-01/MIR-02. Verification confirmed that emit_spill
(lower.rs:399-441) writes the same root list into BOTH the shadow-frame slot
and debug_frame.locals[slot].value, and that liveness.rs:253-263 deliberately
includes CheckFault as a debugger-only spill point. MIR-01's dead-slot clearing
writes 0 into those slots and MIR-02's shrinking omits them, so either one
landing before the split silently breaks the two passing debugger tests above.
MIR-09 needs MIR-14's single symbol table (S2) and P0-04's Unit sentinel (S3),
because its regression asserts the fault path returns a valid Unit.

**Exit criteria**

Un-ignore and pass
local_dead_after_its_last_use_is_not_rooted_at_a_later_safepoint
(liveness.rs:385), exact_roots_shrink_between_two_safepoints_in_one_block
(liveness.rs:462),
empty_element_returning_sinks_fault_instead_of_returning_uninitialized_gc_refs
(adversarial_audit.rs:590). MUST KEEP GREEN, as the gate on MIR-16 landing
first: m11_locals_split_users_and_temps_with_types
(crates/praxis-cli/tests/run.rs:337) and
m11_temp_provenance_shows_materializing_expression (run.rs:371). NEW test
required for MIR-10's verifier (a hand-built Function with a Scalar local in
live_roots, or an out-of-range Jump target, must be rejected).

### S10 — Semantic comparison, nominal schema identity, debugger type recovery

*5 findings · weight 23*

**Goal**

Add a descriptor-level compare callback, give record schemas explicit
nominal-vs-anonymous identity, make collection formatting deterministic, and
drive debugger type recovery from the registry.

| Finding | Sev | Effort |
|---|---|---|
| P0-12 | P0 | L |
| DBG-01 | P1 | S |
| DBG-02 | P1 | M |
| RT-12 | P1 | L |
| RT-16 | P2 | M |

**Ordering**

DESIGN DECISION REQUIRED before P0-12 can be implemented — see
sequencing_hazards. Depends on S1 (id uniqueness) and S7 (descriptors must be
truthful before comparison dispatches on them). TypeDescriptor gains a
`compare` field here, touching all ~21 descriptor consts plus 4 test
descriptors; land that field before RT-13's EnumSchema work in S18 or the same
initializers conflict twice. DBG-01's exhaustive match with no catch-all is
what makes future descriptor additions a compile error in the debugger.

**Exit criteria**

Un-ignore and pass
text_ordering_is_lexicographic_without_payload_reinterpretation
(adversarial_audit.rs:624),
char_ordering_uses_unicode_scalar_values_without_out_of_bounds_reads (:632),
float_heap_entries_use_numeric_order (heaps.rs:194),
ordering_rejects_bool_operands (infer_tests.rs:880),
ordering_rejects_function_operands (:890),
ordering_rejects_composites_without_a_matching_runtime_lowering (:900),
regression_runtime_scalar_descriptors_recover_their_actual_types
(evaluate.rs:497),
regression_runtime_vec_descriptor_recovers_its_real_element_type
(evaluate.rs:485). NEW tests required for RT-12 (two anonymous record schemas
with identical shape but distinct allocations compare equal; two nominal
records with different defs do not) and RT-16 (map/set/counter/heap formatting
is order-stable across runs).

### S11 — TypeDb core: levels, schemes, nominal identity, validated constructors

*8 findings · weight 54*

**Goal**

Fix level clamping together with recursive-placeholder placement and signature
predeclaration; make schemes own their binders; give enums nominal identity
plus args; make type constructors total or fallible.

| Finding | Sev | Effort |
|---|---|---|
| TY-01 | P1 | L |
| TY-02 | P1 | S |
| TY-03 | P1 | L |
| TY-04 | P1 | M |
| TY-06 | P1 | XL |
| TY-07 | P1 | L |
| TY-22 | P1 | M |
| TY-05 | P2 | M |

**Ordering**

HARD BARRIER before S13, S14, S15, S17. TY-01 and TY-22 are the same edit and
must be one unit: TY-01's own analysis says correcting lower_levels requires
moving the recursive placeholder to the declaration-group level, and doing (a)
alone unsoundly generalises every pre-declared signature. TY-02 must precede
TY-03 (the substitution fold is what removes the .expect at generalize.rs:135).
TY-05 precedes TY-06 (payload normalization first), TY-06 precedes TY-04 and
TY-07. Expect wide insta churn in infer_tests.rs and hover_tests.rs from any
scheme change.

**Exit criteria**

Un-ignore and pass
linking_an_outer_var_to_an_inner_type_prevents_inner_generalization
(types_tests.rs:757), instantiation_preserves_non_quantified_variable_identity
(:781), deep_resolve_rewrites_record_field_links (:811),
empty_enum_payload_and_no_payload_are_equivalent (:836),
forward_call_is_checked_against_later_function_signature (infer_tests.rs:1152).
The currently-PASSING test generalized_var_state_is_marked (types_tests.rs:301)
asserts the arena flag TY-03 deletes and must be rewritten to assert scheme
binders instead.

### S12 — Parser grammar: wildcard token, statement separators, struct-literal suppression

*4 findings · weight 13*

**Goal**

Emit UNDERSCORE for a lone underscore, make statement separation explicit and
newline-aware, and thread struct-literal suppression as a parameter that
brackets reset.

| Finding | Sev | Effort |
|---|---|---|
| FE-02 | P1 | S |
| FE-04 | P1 | L |
| FE-06 | P1 | M |
| DBG-03 | P2 | S |

**Ordering**

HARD BARRIER before S16 (FE-02 is the entirety of HIR-05's fix; no HIR change
is needed). Internal order: FE-04 before FE-06, because the match-arm half of
FE-06 relies on FE-04's newline/comma arm separation — landing FE-06's
Allowed-in-arm-bodies change first mis-parses `match x { A => Point { x: 1 } B
=> ... }`. DBG-03 consumes FE-01's shared ident predicate from S2. Two DESIGN
DECISIONS gate this stage (FE-02 discard bindings, FE-04 separator strictness)
— see sequencing_hazards. Highest snapshot churn of any stage: ~40 insta tests
in parse.rs plus every .px fixture must be re-checked for same-line statements.

**Exit criteria**

Un-ignore and pass regression_lone_underscore_has_its_dedicated_token_kind
(lex.rs:617), regression_same_line_statements_require_a_semicolon
(parse.rs:1512), regression_semicolons_separate_top_level_statements
(parse.rs:1522), regression_newline_terminates_a_bare_return (parse.rs:1536),
regression_parenthesized_record_literal_is_valid_in_a_condition
(parse.rs:1574), regression_match_arm_may_return_a_record_literal
(parse.rs:1585). The currently-PASSING test
sanitize_rejects_digit_leading_and_punct (evaluate.rs:476-482) PINS DBG-03's
defective behaviour and must be rewritten, not extended.

### S13 — Annotations honored, declaration passes, mutability and scope discipline

*10 findings · weight 26*

**Goal**

Make tuple/fn/enum annotations reachable, seal a type environment before the
value pass, delete the inferer's disconnected scope tree, and add
mutability/arity/redeclaration diagnostics.

| Finding | Sev | Effort |
|---|---|---|
| TY-08 | P1 | L |
| TY-09 | P1 | S |
| TY-10 | P1 | M |
| TY-11 | P1 | S |
| TY-13 | P1 | L |
| TY-14 | P1 | S |
| TY-15 | P1 | S |
| TY-23 | P1 | S |
| TY-24 | P1 | S |
| TY-12 | P2 | S |

**Ordering**

Depends on S11 (TY-07's validated constructors, TY-06's nominal defs). Internal
order: TY-10 before TY-09 (the sealed TypeEnv is what makes the collapsed
lookup total); TY-13 before TY-14 and TY-15 (both need infer_assign to resolve
the correct symbol through refs rather than the disconnected scope tree at
infer.rs:645). This stage is the audit's biggest source of newly-REJECTED
valid-looking programs — schedule ONE corpus triage pass over
crates/praxis-cli/tests/fixtures, crates/praxis-codegen-cranelift/tests/jit.rs
and tests/aoc-corpus at the end of the stage, not per finding.

**Exit criteria**

Un-ignore and pass tuple_parameter_annotation_is_enforced (infer_tests.rs:607),
function_parameter_annotation_is_enforced (:619),
tuple_return_annotation_is_enforced (:631), user_enum_annotation_is_enforced
(:641), function_typed_record_field_annotation_is_enforced (:653),
function_typed_enum_payload_annotation_is_enforced (:667),
forward_struct_annotation_is_enforced (:678),
value_binding_name_is_not_accepted_as_a_type (:689),
malformed_collection_type_arity_is_rejected (:699),
local_var_reassignment_preserves_its_type (:711),
reassignment_to_let_is_rejected (:721),
compound_assignment_requires_a_numeric_target (:731),
analyzing_nested_function_never_panics (:1175),
duplicate_function_declarations_are_rejected (:1163).

### S14 — Control flow: bottom type, contexts, joins, loop values

*6 findings · weight 26*

**Goal**

Move Never out of ScalarType and add TypeDb::join; add fn/loop context stacks;
fix block tails, if-without-else, return unification and loop break values.

| Finding | Sev | Effort |
|---|---|---|
| TY-16 | P1 | S |
| TY-17 | P1 | M |
| TY-18 | P1 | M |
| TY-19 | P1 | L |
| TY-20 | P2 | M |
| TY-21 | P2 | L |

**Ordering**

HARD internal ordering: TY-19 before TY-17. TY-17 requires an else-less `if` to
be Unit, but `if c { return 1 }` and `if c { panic(..) }` must keep
typechecking — that needs Never absorbed by a join, which is TY-19. Landing
TY-17 first rejects valid programs. TY-19 and TY-20 must both precede TY-21 (it
needs join and the loop stack). TY-18 and TY-20 share the same fn_stack and
TY-20 and TY-21 share the same loop_stack — build each once. TY-21 needs a
DESIGN DECISION on loop-break-value semantics.

**Exit criteria**

Un-ignore and pass never_branch_coerces_to_the_other_branch_type
(infer_tests.rs:800),
control_flow_terminators_require_a_legal_enclosing_context (:775),
expression_before_trailing_statement_is_not_the_block_value (:763),
if_without_else_cannot_produce_the_then_value_type (:741),
early_return_value_must_match_the_function_result (:753),
expression_loop_uses_its_break_value_type (:790). MIR
build.rs:1620/1629/2877/2888 must change from `if let Some(ctx)` to an expect
justified by inference.

### S15 — Per-use types into HIR and MIR, then monomorphization

*7 findings · weight 37*

**Goal**

Record every inferred use-site type, make lowering a pure reader, and
substitute rather than follow during specialization; then enable the verifier's
no-Opaque rule.

| Finding | Sev | Effort |
|---|---|---|
| HIR-01 | P1 | XL |
| HIR-08 | P1 | S |
| MONO-01 | P1 | L |
| MONO-02 | P1 | M |
| MONO-03 | P1 | M |
| HIR-02 | P2 | S |
| HIR-09 | P2 | S |

**Ordering**

This is the stage the audit's ordering understates. P0-02's own blast-radius
analysis concedes that several sites (pipeline accumulators
build.rs:2409/2427/2439, source_item_ty build.rs:1863) have NO correct type
available until HIR-01 carries inferred per-use types. P0-02 in S3 therefore
cannot close its own invariant; it lands the representation and defers those
sites to Opaque, and only here can the verifier's totality rule be enabled.
Depends on S11 (TY-02/TY-03 instantiation identity) and S14. MONO-01 depends on
TY-04's record/enum side-table recursion. HIR-08's shared TypedExpr::children()
walker retires the same bug class in mono.rs rewrite_expr and build.rs
collect_closures_expr.

**Exit criteria**

Un-ignore and pass
lowered_polymorphic_call_result_uses_the_callsite_instantiation
(infer_tests.rs:1061),
lowered_generic_method_result_uses_the_receiver_instantiation (:1092),
immediately_invoked_closure_boxes_its_mutable_capture (:1322),
specialized_clone_carries_concrete_types_throughout (mono.rs:584),
zero_argument_generic_result_is_specialized_from_use_context (mono.rs:625),
enum_payload_types_participate_in_monomorphization_cache_key (mono.rs:641).
ALSO: turn ON the MIR verifier rule from MIR-10 that rejects any
descriptor-producing instruction whose ty is MirType::Opaque, and convert the
~35 sites P0-02 marked Opaque in S3 to Known. NEW test required for HIR-09
(CaptureError deleted; the currently-PASSING mutable_capture_records_error at
capture.rs:345 asserts the bug and must be deleted or inverted).

### S16 — Records, patterns, exhaustiveness, enum constructors

*5 findings · weight 25*

**Goal**

Resolve constructors by symbol not text, make record literals exact by
construction, reject unknown variant patterns, and replace the ad-hoc
exhaustiveness check with a usefulness matrix.

| Finding | Sev | Effort |
|---|---|---|
| HIR-03 | P1 | M |
| HIR-04 | P1 | L |
| HIR-05 | P1 | M |
| HIR-06 | P1 | L |
| HIR-07 | P1 | M |

**Ordering**

HIR-05 is discharged entirely by FE-02 in S12 plus a wildcard-param
representation for `|_|`; do not write an HIR-side fix. HIR-03's
SymbolKind::EnumVariant must precede HIR-07 (which uses it to validate
constructor names at resolution) and HIR-06 (whose matrix needs padded,
arity-exact subpatterns). HIR-06 makes TypedPattern::Wildcard reachable from
source for the first time — verify MIR's lower_match decision tree on a path it
has only ever seen from synthesized fallbacks. Expect new Y120/Y121 to fire on
existing corpora.

**Exit criteria**

Un-ignore and pass lowering_respects_a_local_that_shadows_an_enum_variant
(infer_tests.rs:1123), record_literal_requires_every_declared_field (:1191),
record_literal_rejects_unknown_fields (:1202),
record_literal_rejects_duplicate_fields (:1214),
wildcard_pattern_does_not_bind_a_value_named_underscore (:1225),
nested_enum_pattern_must_cover_payload_constructors (:1235),
duplicate_enum_arm_is_unreachable (:1250),
unknown_enum_variant_pattern_is_rejected (:1264).

### S17 — Constraint channel and capabilities

*11 findings · weight 66*

**Goal**

Add a real constraint representation carried by schemes, bound catalog type
variables, enforce numeric/orderable/hashable, and resolve the prelude phantom
names.

| Finding | Sev | Effort |
|---|---|---|
| RT-08 | P1 | L |
| TY-25 | P1 | S |
| TY-27 | P1 | M |
| TY-28 | P1 | M |
| TY-29 | P1 | XL |
| TY-31 | P1 | M |
| TY-32 | P1 | L |
| TY-33 | P1 | L |
| TY-26 | P2 | S |
| TY-30 | P2 | L |
| TY-34 | P2 | M |

**Ordering**

Depends on S11 (TY-29 reshapes the same Scheme struct TY-03 does — do it in one
reshape, not two) and on S10 (TY-32's heap-ordering half and RT-08 cannot close
without P0-12's compare callbacks). Internal order: TY-29 first (everything
else consumes the worklist), then TY-31's TypePattern::Var bound migration as
ONE commit across 74 sites, then TY-30 and TY-32. TY-30 also needs HIR-01 from
S15 or lowering's second catalog lookup still emits Y110. Three DESIGN
DECISIONS gate this stage — TY-32 hashability, TY-33 remove-or-implement, TY-34
Range. Record that ADR-010's M7 deferral of the §5.4 capability system has
lapsed and supersede its consequence.

**All three gating decisions are ANSWERED (§7), and two went against the
recommendation. Re-size the stage before starting.** D4 confirms the rejection
of mutable collections as keys, as recommended. D5 implements all fifteen
prelude names rather than ten — the six graph helpers get built. D6 implements
`..`/`..=` in full rather than deleting `CollectionCtor::Range`.

The eleven findings above are now the *smaller* part of S17. As answered it is
four units, and only the first is what the finding table describes:

1. **F10's constraint channel + the eleven findings** — the stage as planned,
   in the internal order above.
2. **`panic`, `assert`, `dbg`** — before everything, because `panic` typechecks
   today and then fails to compile.
3. **The eight numeric prelude helpers** — after TY-31, whose numeric constraint
   they want.
4. **The six graph helpers and the `..`/`..=` slice** — each its own
   stage-sized unit with its own exit criteria, and each with sub-decisions §7
   lists as still owed (graph representation; range half-openness, value-ness,
   descending behaviour, endpoint types).

Do not interleave (4) with (1). A graph helper's signature is the hardest test
of the channel's `HasMethod`/`Iterable` constraints, and debugging both at once
is what staging exists to prevent.

**Exit criteria**

Un-ignore and pass polymorphic_equality_rejects_function_instantiation
(infer_tests.rs:922), iterable_constraint_rejects_int_instantiation (:935),
collection_method_constrains_unannotated_receiver_parameter (:946),
sum_requires_int_elements (:960), map_key_must_be_hashable (:974),
mutable_collection_cannot_be_used_as_a_map_key (:985),
mutable_collection_cannot_be_used_as_a_set_element (:1005),
heap_element_must_be_orderable (:1023),
heap_element_requires_a_runtime_compatible_ordering (:1034),
prelude_assert_requires_bool (:910), parse_requires_text_input (:840),
unary_minus_accepts_float_typed_variables (:850), float_remainder_is_rejected
(:860), integer_literal_overflow_is_diagnosed (:870),
mutating_a_structural_key_does_not_break_lookup_by_the_same_value
(dynamic_key.rs:242), mutating_a_collection_key_does_not_break_map_lookup
(adversarial_audit.rs:357). The currently-PASSING numeric_scalars_are_orderable
(capability.rs:261-296) asserts Text/Char orderability and changes meaning.

### S18 — Option contract and enum nominal identity

*3 findings · weight 19*

**Goal**

Give runtime enums a schema with nominal identity, add TypePattern::Option, and
make Map.get and Grid.find return Option instead of a Unit sentinel under a
non-Unit static type.

| Finding | Sev | Effort |
|---|---|---|
| RT-13 | P1 | L |
| RT-14 | P1 | L |
| RT-15 | P1 | M |

**Ordering**

BLOCKED ON A DESIGN DECISION (Map.get contract). This is a source-visible
language change: existing programs using m.get(k) as a bare V stop compiling.
RT-13 must land first and in ONE commit with codegen — EnumPayload gains a
schema pointer, praxis_alloc_enum's signature changes, and the #[repr(C)]
layout crosses the JIT boundary. RT-13 also needs TY-06's canonical Option
def-id (S11), because infer.rs:1688-1697 currently mints a fresh Option enum
per annotation site. Counter's zero-default (§6.2) is deliberate and must NOT
be changed.

**Exit criteria**

Un-ignore and pass map_get_returns_option (infer_tests.rs:1051),
absent_map_get_does_not_return_an_untyped_unit_sentinel (abi.rs:3544),
absent_grid_find_does_not_return_an_untyped_unit_sentinel (abi.rs:3564),
absent_map_get_has_no_unit_under_the_value_type (adversarial_audit.rs:577),
absent_grid_find_has_no_unit_under_a_tuple_type (adversarial_audit.rs:562). NEW
test required: a catalog invariant sweep asserting every MethodEntry with
can_fault:false and a non-Unit result has a runtime symbol that cannot return
Unit (rg 'unit_sentinel' in abi.rs = 56 sites; most are legitimately -> Unit).

### S19 — Input-parser compile pipeline

*11 findings · weight 19*

**Goal**

Decode templates as UTF-8, give captures their real parsers, make constructor
arity and separators checked by construction, and close the validation gaps.

| Finding | Sev | Effort |
|---|---|---|
| IP-01 | P1 | S |
| IP-05 | P1 | M |
| IP-06 | P1 | S |
| IP-07 | P1 | M |
| IP-09 | P1 | M |
| IP-10 | P1 | S |
| IP-02 | P2 | S |
| IP-03 | P2 | S |
| IP-04 | P2 | S |
| IP-08 | P2 | S |
| IP-11 | P2 | M |

**Ordering**

HARD BARRIER before S20. IP-01's cursor rewrite is the substrate for IP-02,
IP-03, IP-04 and IP-05 — they are all in the same 130-line scan_template and
must be one rewrite, not five patches. IP-05 gates IP-06 and gates IPR-13's
descriptor derivation in S20. IP-10 is a SAFETY-CRITICAL ordering item: its
gating test drives an empty separator into a non-advancing runtime loop, so the
fix (NonEmptySeparator by construction) must land before that test is ever
executed in a shared process. IP-11 depends on S1 — a Float parser atomic walks
straight into the TypeId(5) collision. IP-12 is in S8 with the other arena
work.

**Exit criteria**

Un-ignore and pass regression_unicode_literal_text_is_preserved (scan.rs:282),
regression_trailing_backslash_is_an_invalid_escape (scan.rs:292),
empty_separator_is_rejected_before_plan_construction (validate.rs:326),
repeated_section_tail_cannot_reuse_a_fixed_field_name (validate.rs:341),
mixed_template_capture_kinds_are_preserved (infer_tests.rs:1277),
unknown_template_capture_parser_is_diagnosed (:1290),
unknown_parser_constructor_is_diagnosed (:1300),
optional_rejects_extra_arguments (:1310).

### S20 — Parser runtime cursor and region ownership (single rewrite)

*14 findings · weight 51*

**Goal**

Replace mixed absolute/relative offsets with one Cursor/ByteRegion/Input
representation, bound every child parser, iterate by Unicode scalar, derive
descriptors from the plan, and only then add safepoints.

| Finding | Sev | Effort |
|---|---|---|
| IPR-03 | P0 | M |
| IPR-01 | P1 | L |
| IPR-02 | P1 | M |
| IPR-04 | P1 | M |
| IPR-05 | P1 | M |
| IPR-06 | P1 | L |
| IPR-07 | P1 | S |
| IPR-08 | P1 | S |
| IPR-10 | P1 | M |
| IPR-12 | P1 | M |
| IPR-13 | P1 | M |
| IPR-14 | P1 | L |
| IPR-09 | P2 | M |
| IPR-11 | P2 | S |

**Ordering**

IPR-01 must be a single change covering all 12 relative-return sites and all 5
re-slicing sites; a partial rewrite leaves absolute and relative cursors mixed,
which is the current bug in a harder-to-detect form. IPR-14 MUST BE LAST IN
THIS STAGE and must follow S5: adding safepoints to the parser interpreter
before the native root scope exists converts unbounded growth into
use-after-free, because parser.rs's items/captures/values Vec<GcRef>
intermediates are invisible to maybe_collect. IPR-03 also needs RT-06's
validated Text slice constructor (S7). IPR-13 needs S1, S7 and IP-05 (S19).
IPR-10 and IPR-12 are grammar-semantics changes needing judgement.

**Exit criteria**

Un-ignore and pass
text_slices_in_later_sections_point_at_their_actual_source_bytes
(parser.rs:1463), sections_preserve_text_offsets_into_the_original_input
(adversarial_audit.rs:394),
lines_require_each_child_parser_to_consume_the_whole_line (:403),
lines_rest_is_bounded_to_each_line (:419),
unicode_grid_cells_are_parsed_once_per_scalar (parser.rs:1488),
csv_rest_parser_is_bounded_to_each_token (parser.rs:1513),
consume_ws_space_run_requires_one_or_more_spaces_or_tabs (parser.rs:1540),
single_anonymous_template_capture_uses_its_child_descriptor (parser.rs:1575),
anonymous_word_template_vec_uses_the_text_element_descriptor
(adversarial_audit.rs:428),
template_text_capture_stops_before_the_following_literal (:449),
chars_result_descriptor_matches_the_values_it_contains (:458). NEW test
required for IPR-14 (choice backtracking under allocation pressure must not
reclaim a live intermediate).

### S21 — Pipeline plan representation and per-stage indices

*6 findings · weight 28*

**Goal**

Make the pipeline chain recursive, give each stage its own dense counter,
thread a distinct pipeline exit, carry real tuple types, and support dynamic
take/skip bounds.

| Finding | Sev | Effort |
|---|---|---|
| MIR-03 | P1 | M |
| MIR-04 | P1 | L |
| MIR-05 | P1 | M |
| MIR-06 | P1 | L |
| MIR-07 | P1 | M |
| MIR-08 | P1 | M |

**Ordering**

All six touch the same ~1290-line M8-WS11 block (build.rs:1650-2937) and must
be one workstream. MIR-06's Chain refactor is the substrate: it deletes the
unreachable! arm structurally rather than guarding it, and MIR-07's per-stage
counters and MIR-08's PipelineExit both thread through the recursive emitter it
introduces. MIR-04 and MIR-07 are the same dense-counter change applied at two
nesting depths — one commit. MIR-05 needs P0-02's MirType from S3 to carry a
real tuple type rather than Type(0), and its enumerate/zip regressions are the
ones deliberately excluded from S3's exit criteria.

**Exit criteria**

Un-ignore and pass dynamic_take_argument_does_not_silently_lower_to_unit
(build.rs:3870), dynamic_skip_argument_does_not_silently_lower_to_unit
(build.rs:3901), enumerate_tuple_allocation_carries_a_real_two_element_type
(build.rs:4047), zip_tuple_allocation_carries_a_real_two_element_type
(build.rs:4079), enumerate_materializes_index_and_element_tuple_payloads
(adversarial_audit.rs:142), zip_materializes_both_tuple_elements (:160),
take_after_filter_counts_filtered_elements_not_source_indices (:179),
skip_after_filter_counts_filtered_elements_not_source_indices (:189),
zip_after_filter_uses_dense_filtered_positions (:199),
position_after_filter_reports_the_filtered_sequence_index (:213),
two_flat_map_stages_compose_without_a_compiler_panic (:227),
take_after_flat_map_counts_the_global_flattened_stream (:237),
position_after_flat_map_uses_the_global_flattened_index (:247),
any_after_flat_map_short_circuits_the_whole_pipeline (:257).

### S22 — No action

None. This stage is empty: all 139 verified findings are CONFIRMED (136) or
PARTIAL (3 — TY-24, MIR-10, IP-11). No auditor returned REFUTED, ALREADY-FIXED
or UNCERTAIN. The three PARTIAL findings are assigned to real stages (TY-24 to
Stage 13, MIR-10 to Stage 9, IP-11 to Stage 19) because in each case the
verification confirmed a genuine underlying defect while narrowing the audit's
overstated claim: TY-24's silent overwrite is real but ScopeTree::bind's
doc/signature mismatch is the actual defect; MIR-10's verifier is absent but
the scalar-across-safepoint rule would initially fail on the eager lower_seq_*
lowerers; IP-11's missing atomics are real but `uint` has no runtime descriptor
and should not ship.

---

**S22 closes the audit's stages. S23–S26 below are §4.1's** — the defects found
while executing this plan. They come after the audit's stages by numbering only,
not by priority: **S24 holds the repair's last P0**, and S23 depends on nothing
at all, so either may be taken whenever an earlier stage is blocked on a
decision.

### S23 — Independent hardening, round two (runs parallel with every other stage)

*4 findings · weight 4*

**Goal**

The repair-discovered defects that depend on nothing and block nothing. S2's
role, for §4.1's rows.

| Finding | Sev | Effort |
|---|---|---|
| REP-02 | P2 | S |
| REP-11 | P2 | S |
| REP-12 | P2 | S |
| REP-13 | P3 | S |

**Ordering**

None. Each is independently landable and none touches a `#[repr(C)]` layout or
the ABI version. Take them whenever a stage is blocked on a decision.

**Exit criteria**

NEW tests, one per finding — none of these has a gate today.
REP-02: `gc_alloc`'s width comes from the descriptor, so a caller cannot pass a
payload of the wrong width — a compile-time property, gated by the signature
plus a test that every `scalars::*` descriptor's declared payload type matches
what its allocator writes.
REP-11: `1_000` is `1000` and `1_0_0` is `100`, in expression *and* pattern
position; and whichever of lexer-accept / strip-delete is chosen, the other side
has no dead code left (grep for the strip).
REP-12: every `.px` under `tests/` runs through `praxis run` in a Rust test —
this is the missing coverage that let the break sit, and it belongs in the same
commit as whichever way REP-12 is answered.
REP-13: `TypeDb::render` of a nullary collection is `Range`, not `Range[]`, and
a unary one still renders its argument.

### S24 — Function values

*1 finding · weight 8*

**Goal**

Make a bare `fn` name in value position mean something, rather than lowering to
`Unit` and taking the host down when it is called.

| Finding | Sev | Effort |
|---|---|---|
| REP-01 | P0 | M |

**Ordering**

BLOCKED ON A DESIGN DECISION (**D15**: closure value, or compile error). This is
the only P0 left in the repair and it is a host abort reachable from a program
that passes `praxis check`, so it should be scheduled ahead of S25 and S26
whichever way D15 goes. If D15 says "closure value", the fix is in
`lower.rs`'s path-expression lowering and reuses `praxis_alloc_closure` with an
empty environment — a top-level `fn` captures nothing, so the env is `[]` and
the existing calling convention already fits. If D15 says "compile error", the
fix is in inference and is `S`, not `M`.

Note that a graph helper (ADR-060) already contains this at *its* boundary: the
helpers check the descriptor and raise `TypeMismatch` rather than jumping. That
is containment for one caller, not a fix, and it should not be generalized into
one — the bug is that the name evaluates to `Unit` at all.

**Exit criteria**

NEW tests. `let f = double\n f(3)` runs and answers `6` (or is rejected with a
named diagnostic, per D15) — through a `let`, through a parameter of declared
function type, and through a graph helper's neighbour argument, which is the
third way to reach it. Plus a `jit.rs` gate: whatever D15 chooses, **no program
that passes `praxis check` may abort the host**.

### S25 — Grammar completion

*4 findings · weight 12*

**Goal**

Close the source-syntax gaps the design doc's own programs need: the logical
connectives, tuple element access, explicit constructor type arguments, and
record/tuple patterns.

| Finding | Sev | Effort |
|---|---|---|
| REP-07 | P1 | M |
| REP-08 | P1 | M |
| REP-09 | P2 | M |
| REP-10 | P2 | L |

**Ordering**

REP-07 and REP-08 first: **§3.3's representative program needs both**, and with
REP-09 it compiles — which is the readable milestone for this stage. **(Measured
at `bb3bc43`, with REP-07 and REP-08 landed: it also needs REP-16 — there is no
`m[key]` syntax at all and §3.3 writes `counts[point] += 1` — and REP-17, a
trailing comma in an argument list. Both are in §4.1; **REP-17 has since landed**.
Everything else in the program works.)** REP-07 is
not only grammar: `&&`/`||` short-circuit, so they lower to branches and not to
a call, and the precedence goes *below* comparison and above `..` (which S17
already inserted at `bp(3, 4)` — **read the whole table**, every number moved
once already). REP-10 last: it is the largest and `exhaustive.rs` already
handles the `Closed`-with-one-constructor shape it produces, so it is a parser
and lowering change rather than a checker one.

**Exit criteria**

NEW tests throughout; none of these has a gate.
REP-07: precedence against comparison and `..` both ways, short-circuit observed
(`false && panic("x")` does not fault), and `||`'s dual.
REP-08: `p.0` on every tuple arity, in expression and in a closure body, plus
that `p.0` on a non-tuple is a named diagnostic and not a field lookup.
REP-09: `Counter[(Int, Int)]()` parses and means what the annotation says, and
disagreeing with inferred use is a `Y001`.
REP-10: a record pattern binds every field, a tuple pattern binds by position,
and the exhaustiveness signature for both becomes `Closed` — so a `match` on a
record with one arm and no `_` is accepted where it needed a `_` before.
**And the acceptance criterion for the stage: §3.3's representative program,
verbatim from the design doc, compiles and runs.**

### S26 — Declaration, pattern and inference gaps

*5 findings · weight 11*

**Goal**

The repair-discovered defects that silently accept a wrong program or reject a
right one.

| Finding | Sev | Effort |
|---|---|---|
| REP-03 | P1 | M |
| REP-05 | P1 | S |
| REP-06 | P1 | S |
| REP-04 | P2 | M |
| REP-14 | P2 | M |

**Ordering**

**REP-03 and REP-04 are one fix and must land together** — they are the same
defect from two ends. `iter_item` answers an unresolved receiver with *itself*,
which is why a `for` over an unannotated parameter pins the parameter to its own
element type; answering with a **fresh** item variable is what makes the deferred
`Iterable { item }` constraint have two things to relate, which is what REP-04
needs to be checkable at all. Doing either alone leaves the other unfixable in
the same shape. Land them before the rest: this is the constraint channel, and
S17's ADR-057 is the map.

REP-14 is BLOCKED ON A DESIGN DECISION (**D17**), but only for its wording; a
fresh variable in a recursive member is wrong under every answer and the *detect*
half can land first. REP-05 needs a new diagnostic code — `Y124` is next free
(ADR-051) — and REP-06 reuses `N005`, which already says "cannot be declared
inside another function" for `fn`.

**Exit criteria**

NEW tests throughout.
REP-03/04: `fn total(r) { var t = 0\n for i in r { t = t + i }\n t }` is accepted
and `total` generalizes to an iterable of `Int`, for `Vec`, `BitSet` and `Range`;
and an `Iterable` constraint that discharges at a differently-itemed receiver is
*reported*, which is the half that has never had a test.
REP-05: `Wrap(a, b)` against a one-slot variant is `Y124`, and the right arity
still binds and still runs.
REP-06: a nested `struct`/`enum` is `N005` at the declaration, not `N001` at the
use; and the four top-level declaration kinds are untouched.
REP-14: `struct Node { next: Node }` reports rather than registering a variable
(per D17), and a non-recursive forward reference still resolves — TY-10's
`a_type_declaration_is_registered_after_the_types_it_names` is the gate that must
stay green.

## 6. Sequencing hazards

The audit's warning that "later fixes otherwise expose latent unsafe paths" is
correct and specific. These are the concrete cases: each describes a change
that, landed alone, makes the codebase **less** safe than leaving the bug open.
`(VERIFIED)` marks a hazard whose mechanism was confirmed in the code, not
merely inferred.

**H1 (VERIFIED, most severe)**

Parser safepoints before native root scopes. abi.rs:101-114 early-returns when
ctx.roots is null, so today NOTHING is collected during host-driven allocation
or anywhere in the parser interpreter. P0-06's fix deletes that guard. If P0-06
lands without P0-07's native arm, every unrooted Vec<GcRef> in parser.rs
(items/captures/values at :307, :370, :884, :970) and in the grid helpers
(abi.rs:2294, 2446, 2474, 2506, 2618) becomes immediately collectable. This
converts a memory-growth bug into use-after-free on the first collection. P0-06
and P0-07 MUST be one commit (Stage 5), and IPR-14 must not add its safepoints
until that commit lands.

**H2 (VERIFIED)**

P0-08b before Stage 5. Adding maybe_collect to 22 runtime wrappers while
input_source, parse_detail.partial, crash_snapshot and native intermediates are
unrooted starts collecting live objects. The audit flags this, and verification confirms the
mechanism is the same null-guard plus single-RootSet dispatch at abi.rs:112.

**H3 (VERIFIED)**

MIR-01/MIR-02 before MIR-16. emit_spill (lower.rs:399-441) writes ONE root list
into both the shadow-frame slot AND debug_frame.locals[slot].value, and
liveness.rs:253-263 deliberately includes CheckFault as a debugger-only spill
point. MIR-01's dead-slot clearing stores 0 into those slots; MIR-02's
shrinking omits them. Either landing before the debug/root split breaks two
currently-PASSING tests (crates/praxis-cli/tests/run.rs:337
m11_locals_split_users_and_temps_with_types and run.rs:371
m11_temp_provenance_shows_materializing_expression) and makes the crash
debugger render nulls where it previously rendered values.

**H4 (VERIFIED, corrects the audit)**

P0-04 in isolation makes P0-08(b) quieter, not safer. The arithmetic wrappers
already return unit_sentinel(ctx) on fault (abi.rs:566-570, :583-601) and
codegen calls Symbol::IntLoad on that result at lower.rs:877 BEFORE
call_check_fault at :880 — an 8-byte read past a size-0 Unit payload, live
today independent of P0-04. Separately, once P0-04 makes function fault
epilogues return a real Unit instead of iconst 0, a caller that int_loads a
faulted return gets a silently wrong value where it previously got a loud
segfault. P0-04 and P0-08 must be in the same stage (Stage 3).

**H5 (VERIFIED)**

Any descriptor-id guard before P0-01. FLOAT (scalars.rs:242) and TEXT
(text.rs:132) are both TypeId(5). RT-09, RT-10, RT-11, P0-12, DBG-01, DBG-02
and P0-11 all propose id equality guards that are provably no-ops for the exact
pair the audit calls its worst type-confusion. Landing them first ships green
tests that assert nothing. P0-01 is a hard barrier.

**H6 (VERIFIED)**

P0-09 and P0-10 repack the same #[repr(C)] GcHeader, whose size is baked into
JIT-emitted code at lower.rs:1097 (hardcoded size_of::<GcHeader>()). Two
separate repacks means two ABI churns and two chances to desynchronize codegen
from the runtime. One commit.

**H7**

RT-01 before P0-10's sweep poisoning. Reusing swept bump-arena storage without
poisoning the reclaimed header (descriptor=null, heap_id=0) upgrades a stale
GcRef from pointing at dead-but-typed memory to pointing at a LIVE object of a
different type. Strictly worse than the current leak.

**H8**

RT-02's Heap Drop before auditing snapshot ownership. Adding Drop turns any
host that holds a GcRef past Runtime drop into a visible UAF. The debugger
takes CrashSnapshot out of the runtime (crash_snapshot.rs); take_crash_snapshot
consumers in praxis-cli and praxis-debugger must be checked in the same change.

**H9 (VERIFIED)**

Declared dependency CYCLE: P0-02.depends_on=[P0-11] and
P0-11.depends_on=[P0-02]. Resolvable and must be resolved explicitly, or the
planner deadlocks. descriptor_for_type (lower.rs:1630) takes
praxis_types::Type, not MirType, so P0-02 compiles and lands first with `Opaque
=> emit no descriptor` while the Known path keeps its `_ => INT` fallback;
P0-11 then makes the Known path exhaustive.

**H10 (VERIFIED, the audit's largest ordering gap)**

P0-02 cannot close its own invariant in the audit's stage 1. Its blast-radius
analysis concedes that pipeline accumulators (build.rs:2409/2427/2439) and
source_item_ty (build.rs:1863) have no correct type available until HIR-01
carries inferred per-use types — which the audit puts in its stage 4.
Therefore: P0-02 lands the representation in Stage 3 marking those ~35 sites
Opaque explicitly, and the MIR verifier's "no Opaque in a descriptor-producing
position" rule stays OFF until Stage 15. Landing the verifier rule with P0-02
blocks compilation of currently-working programs.

**H11**

TY-01 without TY-22. TY-01's own note states that correcting lower_levels
requires moving recursive-function placeholders to the declaration-group level.
Fixing the comparison alone will unsoundly generalise every pre-declared
signature. TY-01 + TY-22 + TY-03 are one unit.

**H12**

TY-17 before TY-19. TY-17 requires an else-less `if` to unify with Unit, but
`if c { return 1 }` and `if c { panic(..) }` have a Never then-branch and must
keep typechecking. Without TY-19's join absorbing Never, TY-17 rejects valid
programs.

**H13**

FE-06 before FE-04. FE-06's match-arm half changes arm bodies from Suppressed
to Allowed; arm separation then depends on FE-04's newline/comma check. Landing
FE-06 first mis-parses `match x { A => Point { x: 1 } B => ... }`.

**H14 (SAFETY-CRITICAL TEST HAZARD)**

IP-10's gating test empty_separator_is_rejected_before_plan_construction
(validate.rs:326) drives an empty separator into a non-advancing runtime loop.
It must not be executed in a shared test process before the NonEmptySeparator
representation lands. Same discipline applies to the audit's other do-not-batch
tests (invalid descriptor/payload pairings, dangling GcRef,
null-through-non-null-ABI) — the audit explicitly warns against running all
ignored tests in one process.

**H15**

Arena reclamation (Stage 8) before RT-02's drop ordering is settled. The
runtime holds raw *const RecordSchema and *const TupleSchema inside live GcRef
payloads; the generation arena must be dropped AFTER heap teardown or
finalization dereferences freed schemas.

**H16**

Any new runtime symbol before P0-13/MIR-14. P0-12 adds praxis_struct_cmp,
MIR-09 adds praxis_raise_empty_collection, P0-08 adds two non-allocating raise
symbols. Without the single manifest, each must be added to five places (Symbol
enum, arity-derived signature, module.rs list, symbols.rs resolver, MIR string
literal) and MIR-14 confirms module.rs registration is ALREADY incomplete with
a dlsym fallback masking it.

**H17**

Single ABI version bump per stage. P0-06/P0-07 (native_roots), RT-03
(true_ref/false_ref), MIR-16 (DebugLocal layout), RT-13 (praxis_alloc_enum
signature) all change RUNTIME_ABI_VERSION (abi.rs:48) /
COMPILER_EXPECTED_ABI_VERSION (abi.rs:70). Batch per stage; independent bumps
desynchronize generated code from the runtime.

**H18**

Tests that PIN current defects and must be rewritten, not merely unignored —
these will fail as regressions if treated as "should still pass":
generalized_var_state_is_marked (types_tests.rs:301, asserts the arena flag
TY-03 deletes), sanitize_rejects_digit_leading_and_punct (evaluate.rs:476-482,
asserts DBG-03's defective renaming), mutable_capture_records_error
(capture.rs:345, asserts HIR-09's obsolete error),
numeric_scalars_are_orderable (capability.rs:261-296, asserts Text/Char
orderability TY-32 changes), and the vec-adopts-first-descriptor assertions
near adversarial_audit.rs:284 that P0-11 inverts.

## 7. Design decisions required

These are not implementation choices — each changes what the language means,
which programs compile, or what an ADR says. Every one needs an answer from the
repo owner, and each should produce an ADR. **D1, D3, D5 and D15 block their
stage outright**; the rest can be decided while earlier stages are in flight.

D15, D16 and D17 arose during the repair rather than from the audit, and belong
to §4.1's stages; they are at the end of this section.

> **Answers, added as they arrive.** The numbered text below each heading is the
> original question and is left exactly as written; an **ANSWERED** paragraph
> under it is the repo owner's decision. An answer here is a decision, not an
> ADR — the ADR is written by the session that *lands* it, which is why
> `docs/decisions/` has one per implemented decision and not one per answer.
> Answered so far: **D1**, **D2** (ADR-053), **D3** (ADR-045), **D4**, **D5**
> (ADR-060 settles the graph representation D5 left owed), **D6**, **D7**/**D8**
> (ADR-049), **D9** (ADR-042), **D10**, **D11**, **D12**, **D13** (ADR-051),
> **D14** (ADR-040), **D15** (ADR-061), **D17** (ADR-063). Still open: **D16**
> alone.
>
> **D1, D10, D11 and D12 were answered together (2026-07-31)**, in the session
> that started S18. That is four of the five remaining questions in one sitting,
> and it is deliberate: S18 needs D1, S19 needs D10, S20 needs D11 *and* D12, and
> the three stages left after S18 are the whole of what the repair has not
> started. Answering them one stage at a time would have stopped the work three
> more times to ask the same kind of question.

**D1 (Stage 18, blocking)**

Map.get and Grid.find contract: Option[V] per §5.7/§4.7, or keep V-with-Unit?
Source-visible; breaks every program using m.get(k) as a bare V. Also decide
whether Counter.get keeps its zero-default (§6.2 says deliberate — recommend
yes, unchanged).

**ANSWERED — `Option[V]`, and an empty `min`/`max` faults.** Three parts:

1. **`Map.get` and `Grid.find` answer `Option[V]`.** The design doc had already
   decided this and the implementation simply never followed: §5.7 writes
   `Map[K,V].get(K) -> Option[V]` as a signature, and §4.7 says `Option[T]`
   "represents normal domain-level absence. It is not an error channel" — which
   is exactly the case a missing key is. There was no second candidate worth
   weighing: keeping V-with-Unit means keeping a value whose *static* type says
   `V` and whose runtime representation is a `Unit` under it, which is the
   soundness hole RT-14/RT-15 are, not a contract.
2. **`min`/`max` on an empty sequence fault**, rather than answering `0`. MIR-09
   already gave the three *seeded* sinks (`reduce`, `min_by`, `max_by`) a fault
   for the same reason, and the only thing that distinguished `min`/`max` was
   that their accumulator happens to be a scalar seeded to `0` — an
   implementation accident, not an answer. `0` is a *wrong* answer rather than a
   missing one: `[].min()` is not zero, and a puzzle that reduces over a filtered
   sequence silently reads the empty case as a real minimum. They are not made
   `Option` either, which was the third candidate: `Option` is right where absence
   is ordinary (a key may be missing), and wrong where it is a caller mistake
   every single call site would have to unwrap.
   `adv_pipeline_empty_source_min_is_zero` is a §8.2 bug-pinning test and is
   rewritten by the stage that lands this.
3. **`Counter.get` keeps its zero default**, unchanged — the recommendation, and
   §6.2 says it is deliberate. A counter's absent key *has* a defined answer, and
   it is the one every counting program wants.

**D2 (Stage 14)**

Loop break-value semantics: does `loop` with no reachable break yield Never or
Unit? Does a value `break` into `while`/`for` become a type error (recommended:
yes, via split LoopCtx flavours)?

**D3 (Stage 10, blocking P0-12)**

NaN ordering rule, and whether composites (Text, tuples, records, collections)
are orderable at all. Needs a new ADR superseding ADR-026's ordering sentence.
Also resolve the global contradiction: abi.rs:1256-1259 documents "compare by
TypeId, not by pointer, because const descriptors may be duplicated across
crate boundaries", while P0-12 and RT-11 both want pointer identity. If
duplicated consts are real, force single instantiation (pub static, not pub
const).

**D4 (Stage 17)**

Hashability: mutable collections as Map keys are accepted today. Rejecting them
(TY-32/RT-08) breaks existing programs. Confirm the rejection and its
diagnostic wording (§5.4: never name the capability).

**ANSWERED — reject them.** A mutable collection used as a `Map` key or a `Set`
element is `Y014`. This is the recommendation, confirmed.

The precedent the answer turned on: **Python rejects mutable containers as keys
outright** — `list`, `dict` and `set` set `__hash__ = None`, so `{[1,2]: "x"}`
is a `TypeError`, and a `tuple` is hashable only if every element is. Ruby and
most dynamic languages agree; Java permits it and documents the result as
unspecified once a key mutates. **Rust is the counterexample that does not
transfer**: `HashMap<Vec<i32>, V>` is legal only because the borrow checker
makes mutating a key the map still holds impossible. Praxis has `var` mutation
and no borrow checker, so it has Python's exposure without Rust's guardrail.

Which types: the rule is **mutability**, not container-ness. `Vec`, `Map`, `Set`,
`Deque`, `Grid`, `Counter`, `MinHeap`/`MaxHeap` and `BitSet` are out; scalars,
`Text`, tuples, records and enums stay in **structurally** — a tuple or record is
a key iff every component is, which is Python's `tuple` rule and is already how
`supports_eq` recurses. `HashStable` in F10's `CapKind` is the name for this
narrower capability; `Hash` alone is what `supports_hash` answers today, and the
two are not the same predicate. That distinction is the finding: `supports_hash`
is *literally* `supports_eq` right now.

Wording, per §5.4 (never name the capability): say what the program did and why
it cannot work — a value that can change after it is stored cannot be found
again — and name the concrete type. Never "not hashable", never "capability",
never "trait".

**D5 (Stage 17)**

TY-33 remove-or-implement for 15 phantom prelude names. Recommend implementing
panic/assert/dbg/abs/sign/min/max/clamp/gcd/lcm and DELETING the six graph
helpers (bfs, bfs_distance, dfs, dijkstra, a_star, flood_fill) until a
milestone owns them. `panic` must land first — it typechecks today and then
fails to compile.

**ANSWERED — implement all fifteen.** Against the recommendation: the six graph
helpers are *not* deleted, they are built. `panic` still lands first, for the
reason the question gives.

**This makes S17 several sessions, not one.** The six graph helpers are a graph
library — BFS, weighted shortest path, A\* with a heuristic, and flood fill —
and by this plan's own weights that is more work than the other ten findings of
S17 combined. Sequence it as four units, each returning the suite to green:

1. `panic`, then `assert`, then `dbg`. `panic` is the hole every other name is
   built over; it typechecks today and fails at compile.
2. The eight numeric helpers — `abs`, `sign`, `min`, `max`, `clamp`, `gcd`,
   `lcm`. These want TY-31's numeric constraint, so they follow the constraint
   channel rather than preceding it.
3. The six graph helpers, as their own stage-sized unit with its own exit
   criteria. They need a graph *representation* decision first — what a caller
   passes as the neighbour function, and whether `dijkstra`/`a_star` take edge
   weights as a closure or a `Map`. **That decision is not in this plan and is
   owed before the unit starts.**
4. Everything else S17 owns.

Do not interleave (3) with the constraint channel: a graph helper's signature is
where the channel's `HasMethod`/`Iterable` constraints get their hardest test,
and debugging both at once is what the staging exists to prevent.

**D6 (Stage 17)**

TY-34 Range: delete CollectionCtor::Range (8 sites, mechanical) or implement
the full `..`/`..=` vertical slice (XL). Do not leave the middle state.

**ANSWERED — implement the full slice.** Against the 8-site deletion: `..`/`..=`
becomes real syntax.

It is a vertical slice and every layer is a separate commit: lexer tokens, the
Pratt-loop precedence for a binary `..` (below comparison, above the statement
separator — and it interacts with FE-04's newline rule, so `1 ..\n5` needs a
decision of its own), an AST node, the type rule, MIR lowering, and runtime
iteration. Two things already exist and are the anchors: `CollectionCtor::Range`
in `praxis-stdlib`'s type patterns, and `capability::iter_item`'s
`(CollectionCtor::BitSet, _) | (CollectionCtor::Range, _) => Int` arm, which
already says a range yields `Int`.

Sub-decisions this answer does **not** settle, all owed before the slice starts:
whether `a..b` is half-open (recommend yes, with `..=` inclusive — it is what
`iter_item`'s `Int` element already implies and what every neighbouring language
does); whether a range is a first-class *value* or only a `for`-header form
(recommend value, since `CollectionCtor::Range` already makes it a type);
whether a descending range is empty or counts down (recommend empty, matching
Python and Rust, since counting down silently is the bug half the time); and
whether the endpoints may be any numeric type or `Int` only (recommend `Int`
only until TY-31's numeric constraint lands).

**D7 (Stage 12)**

FE-02: after `_` becomes UNDERSCORE, is it still legal in `let _ = f()`, `fn
g(_)`, `|_| 0`? No .px fixture uses them and neither §4.1 nor §4.2 mentions
them. Decide explicitly rather than accidentally.

**D8 (Stage 12)**

FE-04: exactly where a newline terminates an expression versus continues it. `1
+\n2` must still parse; the rule must apply only at statement boundaries and at
break/return's optional-value decision, never mid-Pratt-loop.

**D9 (Stage 7)**

What the JIT does when descriptor_for_type returns Err: fail the compile with a
diagnostic (correct) or fall back (reintroduces the bug).

**D10 (Stage 19)**

How much of the parser-expression grammar a template capture body may contain
(§7.3 shows only atomics; the scanner's }-scan at scan.rs:80-82 cannot handle a
nested brace).

**ANSWERED — a capture body is a full parser expression.** `{items:csv(int)}`,
`{x:optional(int)}` and a nested `{g:choice(…)}` are all legal, and the scanner
learns brace-, paren- and backtick-aware depth tracking instead of scanning to
the first `}`.

The question reads as "how much grammar do we allow", but the design doc had
already spent it: **§7.7's own monkey example writes
`` `  Starting items: {items:csv(int)}` ``**, and §7.8's derivation table gives a
result type for every parser including the nesting ones. So "atomics only" is not
a smaller language, it is a language that cannot run the document's own example.
The middle option — atomics plus one call level — buys nothing: the bounded scan
it allows is *the same scan* as the unbounded one, because tracking depth to 1 and
tracking depth to n are one loop with one counter, and refusing depth 2 would then
be an arbitrary rule the scanner has to carry a diagnostic for.

The real cost is not the grammar, it is that the `}`-scan cannot stay a scan: a
capture body may contain `}`, `)`, `,` and a backtick template of its own, so the
body's extent has to be found by the same cursor discipline IP-01 is introducing
anyway. That is why this is answered *with* IP-01 and not before it.

**D11 (Stage 20)**

IPR-06: does grid(int) mean one digit per cell or a full token? IPR-10/IPR-11:
does `text`/`word` become non-greedy with lookahead to the following literal?

**ANSWERED — a cell parser means what it means everywhere else, and `text`/`word`
stop at the following literal.** Two halves:

1. **`grid(int)` is one whole integer token per cell; `grid(digit)` is the
   per-digit one.** §7.5's `grid(cell_parser)` entry shows `grid(char)` and
   `grid(digit)` as its two examples, and `digit` exists *for* the
   one-digit-per-cell case — if
   `grid(int)` also meant that, `digit` would name nothing. The rule that falls
   out is the one worth having: a cell parser inside `grid` parses a cell exactly
   as it would parse anywhere else, so `int` is an integer token, `digit` is one
   digit, `char` is one scalar. Today `read grid(int)` over `12\n34\n` answers
   cells `[12, 2, 34, 4]` — it reads the token *and* then re-reads its tail — and
   that is not either candidate semantics, it is IPR-06.
2. **`text` and `word` are non-greedy with lookahead to the following literal.**
   A template is a sequence of literals and captures, and a capture that swallows
   the literal after it makes every template with a trailing literal unmatchable —
   `` `{name:text}: {v:int}` `` cannot work under greedy `text`. Greediness is only
   defensible when a capture is *last* in its region, which is the case the
   non-greedy rule already handles (no following literal, so the lookahead is the
   region end). So there is no program the greedy rule serves that the non-greedy
   one does not.

**D12 (cross-cutting)**

Panic-across-FFI policy: catch_unwind at every #[no_mangle] extern "C"
boundary, or panic=abort plus per-wrapper totality proofs. RT-06, RT-07 and the
parser findings all reach a Rust panic from inside extern "C"; this policy
should precede those fixes.

**ANSWERED — per-wrapper totality, with one `catch_unwind` backstop at the
boundary.** Both, and the order matters: totality is the contract and the shim is
the proof that the contract cannot be violated *silently*.

A wrapper is total — a malformed argument raises a fault through the fault
protocol, and no wrapper reaches a Rust `panic!` on any input generated code can
hand it. That is the property the tests gate. On top of it, every
`#[no_mangle] extern "C"` entry point is wrapped once so that a panic which
escapes anyway becomes a fault instead of unwinding into JIT frames — because
unwinding across `extern "C"` into Cranelift-generated frames is undefined
behaviour, and "we proved it cannot happen" is exactly the claim a repair like this
one exists to distrust.

`panic=abort` was the third candidate and is rejected for a reason the crash
debugger makes concrete: M10 exists so that a failing program prints a numbered
backtrace and drops into a REPL. An abort produces neither. A policy that turns
every residual bug into a silent process death would delete the one feature the
milestone before this repair shipped.

**D15 (Stage 24, blocking)**

REP-01: what does a bare `fn` name in **value position** mean? Today it lowers
to `Unit` and calling it takes the host down with a SIGBUS, from a program that
passes `praxis check`. Two answers, and both are defensible:

- **A closure value.** `let f = double` allocates a closure over `double`'s
  entry point with an empty environment — a top-level `fn` captures nothing, so
  the existing `fn(ctx, closure_self, args…)` convention already fits and
  `praxis_alloc_closure` already takes an empty env. `f(3)` then works through
  the `Inst::CallIndirect` path unchanged. **Recommended**: it is what the type
  already claims (a `fn`'s type *is* a `Func`), the runtime shape exists, and it
  is what a caller writing `bfs(start, steps)` obviously means.
- **A compile error.** A bare `fn` name is only legal as a callee; a value needs
  `|n| double(n)`. Cheaper (an inference check, effort `S`), and it keeps
  function values to one representation — but it makes the type system claim
  something the language will not do, and every closure-taking API grows a
  wrapper-lambda tax.

Whichever way it goes, the invariant the stage must establish is the same: **no
program that passes `praxis check` may abort the host.**

**ANSWERED — a closure value.** The recommendation, confirmed. Implemented in
S24; see **ADR-061**.

Two things the answer's implementation found, both recorded in the ADR because
this block's sketch says otherwise:

- **The existing calling convention does *not* already fit.** The environment is
  empty, but a closure's synthetic function takes the closure as a hidden first
  explicit argument and a top-level `fn` does not, so `praxis_alloc_closure` over
  the `fn`'s own address would land every argument one slot to the left. Each
  adapted `fn` gets one **adapter** — `[closure_self, p0 … pn]` forwarding
  `[p0 … pn]` — and that is the whole of the new machinery.
- **A *generic* `fn` has no function value.** Monomorphization is driven by call
  sites and a value has none, so it is `Y018` in inference, with `|x| id(x)` named
  as the remedy. That was not part of the question and is part of the answer.

**D16 (Stage 25)**

Does `assert` take a message, and more generally does the language get
**arity-based overloading or optional parameters**? `assert(cond, "why")` has no
spelling today because a name has one scheme (ADR-056 records this). The same
wall is why the six graph helpers each have exactly one signature and why a
`Grid` convenience overload for `bfs` was not built (ADR-060). Recommend
deciding this once, for the language, rather than per name — and note that
`assert`'s message is the cheapest possible motivating case, so answering it in
isolation would set the precedent by accident.

**D17 (Stage 26)**

REP-14: what does `struct Node { next: Node }` report? ADR-052 already put
*supporting* recursive types out of scope — they are equi- or iso-recursive
types, a language feature, not a repair. This decision is narrower: the
declaration pass currently leaves a **fresh type variable** where the recursive
member should be and says nothing, which is wrong under either answer. So:
report it as an unsupported recursive type (recommended, and it supersedes
ADR-052's silence), or support it and reopen ADR-052.

**ANSWERED as recommended — report it, `N006`. ADR-063.** Supporting recursive
types stays out of scope; ADR-052's *silence* is superseded and amended in place.
Two things the question did not have:

- **A fresh variable is not a neutral fallback.** A variable unifies with
  everything, so `struct Node { next: Node, value: Int }` accepted
  `Node { next: 7, value: 1 }` and **ran** it — one unchecked member per recursive
  declaration, not merely an unnamed one.
- **A declaration that merely waits *behind* a cycle had the same hole.**
  `struct C { a: A }` written above a recursive `A`/`B` pair was left in the
  stalled remainder too, so `C.a` was a variable and `C { a: "text" }` compiled.
  It is not the mistake and must not be reported: the stall now computes which
  remaining declarations reach *themselves*, reports only those, and **resumes**
  the readiness loop for the rest.

The member is still a variable after the report, deliberately — a second report
per use would be the cascade `N004` avoids. The code is `N006` and not `Y019`,
which is ADR-051's own rule ("declaration mistakes go in the Name category").

**D13 (Stage 13/16)**

Diagnostic code allocation. Four groups each need new codes and none
coordinated: taken are Name 1-2, Type 1-6, Type 120-121. Needed across stages:
"value, not a type" (TY-11), collection arity (TY-12), immutable assignment
(TY-14), compound-assign numeric (TY-15), return-outside-fn and
break-outside-loop (TY-20), duplicate declaration (TY-24), nested fn (TY-23),
record literal missing/unknown/duplicate (HIR-04), unknown variant pattern
(HIR-07), Int literal range (TY-28), statement separator (FE-04), plus the I0xx
block for IP-06/IP-07/IP-09/IP-10. One owner must allocate the whole block
before Stage 13 starts.

## 8. Test discipline

The audit shipped 149 `#[ignore = "known bug: …"]` regressions plus 2
`#[cfg(miri)]` boundary tests. They are the acceptance gate: a stage is
complete when its listed tests are un-ignored and green. Four rules govern how
they are used.

### 8.1 Do not batch the ignored suite

`cargo test --workspace -- --ignored` must not be run until S1, S5, S7 and S19
have landed. Individual ignored tests deliberately exercise an invalid
descriptor/payload pairing, a dangling `GcRef`, a null value through a non-null
ABI, and a separator loop that never advances. Run one regression at a time,
after reading its comment. In particular
`empty_separator_is_rejected_before_plan_construction`
(`praxis-input-parser/src/validate.rs:326`) drives a non-advancing runtime loop
and **hangs the test process** until IP-10's `NonEmptySeparator` representation
exists.

### 8.2 Five passing tests assert the bugs

These are green today and will **fail as regressions** if treated as "should
still pass". Each must be rewritten or deleted as part of its stage:

| Test | File | Stage | What it pins |
|---|---|---|---|
| `generalized_var_state_is_marked` | `praxis-types/src/types_tests.rs:301` | S11 | The global arena flag TY-03 deletes. Rewrite to assert scheme-owned binders. |
| `sanitize_rejects_digit_leading_and_punct` | `praxis-debugger/src/evaluate.rs:476` | S12 | DBG-03's defective `_x` renaming, collisions included. Rewrite, don't extend. |
| `mutable_capture_records_error` | `praxis-hir/src/capture.rs:345` | S15 | HIR-09's obsolete "mutable capture unsupported" error. Delete with `CaptureError`. |
| `numeric_scalars_are_orderable` | `praxis-hir/src/capability.rs:261` | S17 | Text/Char orderability, which TY-32 changes. Its meaning inverts. |
| Vec-adopts-first-push-descriptor assertions | `praxis-codegen-cranelift/tests/adversarial_audit.rs:284` | S7 | The retag-on-first-push behaviour P0-11 removes. |

Two more must stay **green** as the gate proving MIR-16 landed before
MIR-01/MIR-02: `m11_locals_split_users_and_temps_with_types` and
`m11_temp_provenance_shows_materializing_expression`
(`praxis-cli/tests/run.rs:337` and `:371`). If root-set shrinking lands before
the debug/root split, the crash debugger renders nulls where it used to render
values and both go red — see H3.

### 8.3 Findings with no gating test

Not every finding has a regression; the audit could not express these safely as
ignored tests. Each needs a test written as part of its fix: P0-05, P0-13,
P0-14, MIR-14, MIR-15, FE-08, IP-13 (S2/S3); DBG-04 (S5); RT-05, P0-08c (S6);
RT-06, RT-07 (S7); DBG-05, MIR-13, IP-12 (S8); MIR-10 (S9); RT-12, RT-16 (S10);
HIR-09 (S15); RT-14/RT-15 catalog invariant sweep (S18); IPR-14 (S20).

### 8.4 Miri

P0-05 and P0-14 are `#[cfg(miri)]` and absent from the normal suite, so those
two fixes currently have no standing gate. Run them explicitly per the audit's
reproduction section, and add a Miri job for `praxis-source` and
`praxis-syntax` to `just ci` once S2 lands.

## 9. Working a stage

A repeatable checklist, because the failure mode across 21 stages is landing a
fix whose foundation is not yet in place.

1. Confirm every prerequisite stage in §2's DAG has landed, and re-read the relevant hazards in §6.
2. Land the stage's foundations (§3) **first**, as their own commits, with the existing suite green. A foundation that compiles with no behaviour change is a good commit.
3. Batch the `RUNTIME_ABI_VERSION` / `COMPILER_EXPECTED_ABI_VERSION` bump **once per stage** (H17). S5, S6, S9 and S18 each change `#[repr(C)]` types that generated code reads.
4. Un-ignore the stage's regressions **one at a time** (§8.1) and fix until each passes.
5. Rewrite any bug-pinning test the stage invalidates (§8.2) in the same commit, with a comment explaining the inversion.
6. Run `just ci` — `fmt-check`, `clippy -D warnings`, and the full suite. The audit's baseline was green; every stage returns to green.
7. Write the ADR when the stage changes a documented decision. At minimum: F1 supersedes ADR-028's "TypeIds 6-19"; D3 supersedes ADR-026's ordering sentence; D1 fixes the `Map.get` contract across §5.7, the catalog and the runtime; F13 closes the §10.5 generation-arena gap; F14 amends ADR-023.

## 10. Provenance

Findings from `docs/handovers/implementation-adversarial-audit-2026-07-28.md`,
re-verified against the working tree at `136ce4b` by thirteen independent
subsystem readers, with two further passes designing the shared foundations
(§3) and the sequencing (§5-§7). 139 findings assessed: 135 confirmed, 4
partial, 0 refuted.

Baseline measured at `136ce4b`: `cargo test --workspace` is **928 passed, 0
failed, 149 ignored**. Those 149 are the audit's `known bug` regressions and
are the acceptance gate for §5; the two `#[cfg(miri)]` boundary tests (P0-05,
P0-14) are additional and not counted here. Every stage must return the suite
to zero failures while converting some of the 149 to passing.
