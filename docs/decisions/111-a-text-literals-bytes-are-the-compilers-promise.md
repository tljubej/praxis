# ADR-111: A `Text` literal's bytes are the compiler's promise, and the input's are the host's

**Date:** 2026-08-03
**Status:** accepted — implemented
**Milestone:** post-M11 repair (REP-67, handover 18 and handover 23 §P-4b)
**Supersedes:** ADR-088 §3, which took the cost and registered this cure. The
rule ADR-088 exists for is untouched — only an input to `Inst::fault_reason`
changed.
**Amends:** ADR-017's fault protocol, which is why this needs a decision record
at all: it changes what a violated wrapper precondition *does*. ADR-108 §3 and
§5 are amended where they name `AllocKind::Text` as un-hoistable.

> **Numbering.** This was assigned 110. While it was being written,
> `praxis-codegen-cranelift/src/lower.rs` acquired four citations of ADR-110 for
> a different decision (inlining `praxis_alloc_bool` and `praxis_alloc_unit` as
> loads). Two ADRs cannot share a number, so this one is 111.

## Context

`praxis_alloc_text`'s `# Safety` block has said the same thing since ADR-017:

> `bytes` must point at `len` valid UTF-8 bytes that remain valid for the
> duration of the call.

And the body then treated a violation of that as a recoverable runtime
condition — `set_fault(ctx, INVALID_TEXT)` followed by
`String::from_utf8_lossy`. **The contract and the implementation contradicted
each other**, and the implementation's half was the expensive one. Because the
body could reach `set_fault`, the manifest row had to say `AllocatesAndFaults`
(REP-45's sweep enforces exactly that), and because the row said so, ADR-088's
rule required an `Inst::CheckFault` after every `Inst::Alloc { AllocKind::Text }`
— **41 sites across the tracked corpus**, every one of them checking for a fault
that cannot happen. ADR-088 §3 spelled out why it refused to carve an exception
into the verifier for them (a rule with a hole in its first arm is not a rule),
registered the cure as REP-67, and took the cost.

**The compiler's bytes are not raw.** A `Text` literal's bytes are a Rust `&str`
unbroken end to end: `Lit::Text(String)` in the typed tree, `AllocKind::Text {
value: String }` in MIR, `Generation::alloc_str` in the backend handing
`embed_text` a `&'static str` whose `as_ptr`/`len` are the two arguments. There
is no step in that chain where the string stops being a `str`. The fault the row
declared was unraisable at every call site generated code emits.

**Two facts have changed since ADR-088 §3 was written, and both make this a
better trade than it was then.**

The cost went *down*, so the speed argument is gone. ADR-088 priced this at "one
check per text-literal evaluation" when a `CheckFault` was a guarded call to
`praxis_check_fault`. ADR-102 landed the next day and made it two loads and a
`brif`. And there is not a single `Text` literal in the benchmark suite — seven
of the eight `.px` benchmarks contain no double quote at all, and `vm.px`'s one
occurrence is inside a comment. **This change cannot move a measured number, and
no claim that it does appears anywhere below.**

The value went *up*, from a source ADR-088 could not have known. ADR-108 landed
today: `box_invariant_literal` hoists a loop-invariant literal allocation into
the loop's preheader, gated on `Inst::can_fault()`, which reads the ABI manifest.
`AllocKind::Text` excluded itself from that hoist for one reason — its row.
Hoisting a `Text` literal out of a loop removes a `Box<str>` allocation, a
memcpy, a GC block and a sweep-time drop **per iteration**, which is a different
order of thing from three instructions.

## Decision

### 1. UTF-8 is `praxis_alloc_text`'s caller's precondition, and the row says `Allocates`

One edit to `crates/praxis-stdlib/src/abi.rs`:

```text
AllocText = "praxis_alloc_text": (Ctx, Ptr, Ptr) -> Gc, Allocates;
```

Everything downstream follows with **no code change**, which is the property
MIR-10 was built for. `RuntimeSymbol::faults()` answers `false`, so
`Inst::fault_reason()` answers `None` for `AllocKind::Text`, so `Builder::emit`
stops pushing the check and `verify::check_fault_observed`'s converse arm starts
*rejecting* one (`VerifyError::RedundantFaultCheck`). Nothing in `build.rs`'s
emit path, `verify.rs`, `liveness.rs` or `lower.rs` needed touching. The removal
is not merely permitted, it is now enforced: restoring the check is a failing
build.

The backend is unchanged and the `Text` allocation is still a GC safepoint —
`liveness::gc_safepoint_slots` matches `Inst::Alloc` structurally, not through
the manifest, so roots are still spilled across the call. What the site loses is
one Cranelift block split and one edge into the fault block.

### 2. A violated precondition aborts; it does not fault

The `std::str::from_utf8` call **stays**, unconditional in every profile, and
its `Err` arm becomes a `#[cold] #[inline(never)] fn text_bytes_are_not_utf8(len: usize) -> !`
that panics. On a violation: panic → `abi_guard!` catches →
`panic_fault_is_observable("praxis_alloc_text")` reads the new `Allocates` row
and answers `false` → the message is printed and the process aborts. That is
byte-for-byte what `praxis_int_load` does with a wrong descriptor, and it falls
out of Decision 1 with no code of its own.

This is ADR-102 §1's shape applied verbatim, and the four alternatives were all
considered and rejected:

- **A `debug_assert` is not a bound.** It compiles out of a release build. What
  would be left in release is a `Box<str>` built over bytes that are not UTF-8,
  which `text_str` later hands out as a `&str`. That is REP-56 exactly — two
  profiles, two answers, and the wrong one is the one users get, with `just ci`
  building neither.
- **`from_utf8_unchecked` / `unreachable_unchecked`** is the same hole with the
  check deleted outright rather than compiled out, and it converts a violated
  precondition from a defined abort into undefined behaviour surfacing at a
  distance — in `text_str`, in the hasher, in `out`.
- **A second ABI entry** (`praxis_alloc_text_unchecked`, with the existing one
  kept as the validating door) costs a manifest row, an address arm, an
  `abi_guard!`, an entry in `every_manifest_symbol_resolves_to_a_distinct_address`
  and a doc; and its sole caller lives inside `praxis-runtime` and never crosses
  the C ABI. A symbol exists to be *called from generated code*; nothing would
  call this one.
- **A type that can only be built from validated bytes** is the "make illegal
  states unrepresentable" answer, and the honest thing to record is that **it is
  already in place on the side where it can be**. `AllocKind::Text` carries a
  `String`, `Generation::alloc_str` answers a `&'static str`, and a Rust `&str`
  is precisely the type whose existence is a proof of UTF-8. What cannot be typed
  is the ABI boundary itself: `praxis_alloc_text` is `extern "C"` taking
  `*const u8` and `usize` because that is what Cranelift can call, and a `&str`
  has no C representation. So the type-level guarantee holds right up to the
  boundary, the boundary re-proves it with a branch, and the branch is
  unconditional because the boundary is where the type was lost.

The cost of keeping the check is a never-taken branch to a cold callee — the
price ADR-102 §1 already established for the inline scalar loads, and the same
argument `int_payload`'s doc makes at length.

**It must not reach `set_fault`, and that is mechanically enforced.** REP-45's
`a_wrapper_that_can_raise_a_fault_declares_that_it_faults` computes a textual
fixed point over `abi.rs`: a body that can reach `set_fault` must belong to a
symbol whose row says it faults. So a refusal spelled as a fault fails the test
and correctly refuses Decision 1. The sweep's explicit name list now includes
`praxis_get_input` and asserts `praxis_alloc_text`'s *absence*, so it proves the
fault relocated rather than merely disappeared — without that pair, deleting the
validation outright would leave the test just as green.

### 3. `praxis_get_input` validates, because it is the one caller holding bytes the compiler did not write

Enumerating the callers is what makes this a two-line change rather than a
refactor. Every other caller of `praxis_alloc_text` in the runtime hands it a
Rust `&str` or `(null, 0)`; `default_cell`'s `B::Text` arm passes the latter,
which the wrapper's own `len == 0` branch turns into the empty slice.

`praxis_get_input` is the exception. Its bytes come from a host's
`InputReader`, which is infallible about I/O by design (`input.rs`: what an
unreadable stdin *means* is the host's question) and says nothing about
encoding. So it validates with `std::str::from_utf8`, raises `INVALID_TEXT`
itself on `Err`, and answers the Unit sentinel. **`GetInput`'s row already said
`AllocatesAndFaults` and `lower_read` already emitted the check**, so
`InvalidText` still diverts *at the `read`* with no MIR change, no manifest
change and no new symbol. The fault moved one frame down the call chain and
nothing above it noticed.

The validation goes strictly **before** the allocation. The SAFETY note there
depends on nothing allocating between the `praxis_alloc_text` call and the
`(*ctx).input_source` store — the result is not a root until that store, so a
collection paced by an intervening allocation could reclaim it. Reordering would
produce an intermittent use-after-free.

`FaultKind::InvalidText` keeps its variant and its discriminant. It is
`#[repr(C)]` and generated code reads it (ADR-102 §3), so removing a variant is
an ABI change; and it still has a producer. Worth recording where that producer
can be reached from: **not from `praxis run`**. `lazy_stdin::read` goes through
`std::io::read_to_string`, which refuses non-UTF-8 stdin and exits 2 before the
runtime sees a byte. `InvalidText` is reachable only from an embedder that
installs its own reader — which is what its test now is.

### 4. `Text` literals join the ADR-108 hoist, and the hoisting code did not change

`box_invariant_literal`'s non-faulting precondition asks `Inst::can_fault()`
rather than carrying a list of allocation kinds, "which is the point of asking
the manifest". Decision 1 is the first time that claim was tested and it held:
**not one line of `box_invariant_literal` changed**, and
`a_text_literal_in_a_loop_is_hoisted_now_that_its_alloc_cannot_fault` is the
inverse of the test that recorded the exclusion — which had said, in as many
words, that P-4b is what would flip it.

What *did* need an edit is the call site, and ADR-108 §5 is the reason: the set
of hoistable literals "is stated at the call sites" rather than in a predicate,
so `lower_lit_gc`'s `Lit::Text` arm had to start calling
`box_invariant_literal`. It passes `None` for the `Const*` companion, because a
`Text` literal's payload rides inside the `AllocKind` instead of through a scalar
local. That is a widening of `box_invariant_literal`'s signature, not of its
decision: there is exactly one `can_fault` gate and it is still stated once.

The shareability half of ADR-108's precondition holds for `Text` unconditionally,
and more simply than for `Float`. A `Text` payload is immutable — `+` allocates a
fresh `Owned` (ADR-085) and no instruction writes one after allocation — `==` is
`text_equals`, a structural byte comparison, and `DynamicKey`'s pointer fast path
is a fast path *for* that comparison, which is reflexive. There is no `Text`
analogue of NaN, so unlike `Lit::Float` this arm passes nothing to filter.

### 5. `praxis_alloc_char` is not included, and the asymmetry is the point

The obvious next step is to do the same to `AllocKind::Char` and delete its check
too. It does not carry: `praxis_alloc_char`'s `INVALID_CHAR` is raised for an
`i64` that is not a Unicode scalar, and such an `i64` arrives from
`Int.to_char()` at run time (ADR-086), not from a literal. The validation has
nowhere to move to, because there is no single caller that owns the untrusted
value — the caller is arbitrary user arithmetic. `Text` was movable precisely
because the untrusted door is one function wide.

## Consequences

- **This buys no measured speed and the ADR does not claim any.** There is no
  `Text` literal in the benchmark suite, so nothing here can appear in a timing
  table. What it closes is REP-67 — registered twice, in ADR-088 §3 and handover
  18 — and a live contradiction between a wrapper's stated contract and its
  behaviour. The one place a real cost is removed is a `Text` literal inside a
  loop, where the hoist takes a `Box<str>` allocation off the per-iteration path;
  no benchmark has one, and a program might.
- **A host calling `praxis_alloc_text` with untrusted bytes now aborts where it
  used to get a recoverable fault.** This is a real reduction in defensive
  behaviour and it is the reason this is ADR-017 territory rather than a tidy-up.
  It is named here rather than glossed. The remedy for such a host is one line —
  `std::str::from_utf8` before the call — and it is what `praxis_get_input` now
  does.
- **`RUNTIME_ABI_VERSION` owes a v19 paragraph, not a bump.** 19 is already this
  batch's number and carries a paragraph per change; this is the fourth. The
  class is v12/v17's — a meaning change with no layout change — and it is owed in
  both directions. Code compiled against v19 emits no check after a text
  literal's `Alloc`, so a v18 runtime that still validated would set a fault into
  a slot nothing at that site reads; and a v19 runtime aborts where a v18 one
  faulted.
- **Three documents stated the old contract by contrast and now state the new one
  by agreement.** `praxis_text_concat`'s doc and ADR-085's Consequences bullet
  both justified their own `Allocates` row *against* `praxis_alloc_text`'s
  validation ("that wrapper validates because it is handed raw bytes"); both now
  say the two rows agree for one reason instead of two. `input.rs`'s module doc
  moves §4.3's UTF-8 judgement from `praxis_alloc_text` to `praxis_get_input`.
- **The one test that had to be rewritten would have crashed the harness, not
  failed.** `alloc_text_reports_invalid_utf8_as_its_own_fault_kind` fed the
  wrapper `[0xF0, 0x28, 0x8C, 0x28]` directly, which after Decision 2 aborts the
  test process. It is replaced by `input_that_is_not_utf8_faults_at_the_read`,
  which drives the same bytes through an installed `InputReader` — the reachable
  path, and the one whose property is worth pinning. Its mutation companion
  `input_that_is_utf8_still_becomes_the_buffer` is required, because a
  `praxis_get_input` that faulted on *every* input would pass the gate alone.
- **The verifier gained no rule and needed none.** ADR-088's `check_fault_observed`
  is unchanged in both directions. This is the change ADR-088 §3 predicted could
  "supersede this decision later without touching the rule", and it is the
  cleanest available evidence that keeping the rule hole-free was right: a
  carve-out for `Alloc { Text }` would now be dead code that nobody remembered to
  delete, and a claim the backend never read.
- **`FaultKind::InvalidText` has exactly one producer and no end-to-end
  coverage, and that is unchanged.** It was already unreachable from `praxis run`
  before this decision, because the CLI validates upstream. Moving the producer
  did not make it less reachable; it made *which* caller can reach it legible.

## Open questions

- **Should `InputReader` become `fn() -> String`?** That deletes the raw-bytes
  state entirely, drops `GetInput`'s row from `AllocatesAndFaults` to
  `Allocates`, removes the last `CheckFault` from `lower_read`, and leaves
  `FaultKind::InvalidText` with no producer at all — the full
  unrepresentable-states version of this decision. Two things argue against
  riding it in here: `input.rs` deliberately places the UTF-8 judgement in the
  runtime per §4.3, and removing a `FaultKind` variant is a `#[repr(C)]` change
  generated code now reads (ADR-102 §3). Its own item.
- **Should `Text` literals be interned per JIT generation, the way ADR-100
  interns small `Int`s?** That is the change that would make a `Text` literal
  genuinely free rather than once-per-loop-entry, and the identity argument in
  Decision 4 carries it verbatim. It is blocked by `GcConst`'s membership rule: a
  constant may not be a compile-time address, because a `praxis_debugger` session
  swaps the `Jit` while keeping the `Runtime`. Getting past that needs a
  per-generation literal table the runtime owns and an index-valued `GcConst`.
- **Is `AllocKind::Char`'s check worth removing by a different route?** Decision 5
  says the validation cannot move. It could instead be *hoisted into the type*:
  `AllocKind::Char { value }` could carry a validated `char` where the literal is
  known, leaving the wrapper's fault for `Int.to_char()` alone. That is a MIR
  shape change and it interacts with ADR-107's interned `Char` table.
