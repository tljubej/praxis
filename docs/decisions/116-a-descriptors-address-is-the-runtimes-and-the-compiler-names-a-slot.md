# ADR-116: A descriptor's address is the runtime's, and the compiler names a slot

**Date:** 2026-08-03
**Status:** accepted — implemented
**Milestone:** post-M11 performance (handover 25 finding F-4.1, handover 26
package W6)
**Amends:** one line of ADR-102. Its inline type proof compared the header's
descriptor word against `iconst &scalars::INT`; it now compares it against
`load [ctx + descriptor_offset(BuiltinTypeId::Int)]`. **Everything ADR-102
argued is preserved and none of it is weakened** — the check is still
unconditional in every profile, still compares the same descriptor, still routes
a mismatch to the same cold block calling the same wrapper, and still refuses
`ScalarKind::Byte` an inline form. What changes is where the address the
comparison uses comes from, and the interesting part is that after this the
compiler does not have one.
**Takes the round's single `RUNTIME_ABI_VERSION` bump**, v19 → v20, on behalf of
four packages that wanted it (handover 26 §5).

## Context

ADR-102 turned a scalar payload read from a call into `load descriptor;
iconst DESC_ADDR; icmp eq; brif; load payload`. On aarch64 the second of those is
not one instruction. A `static` in this binary lives above 2³², so Cranelift
materializes its address in three:

```text
block5:                              ; an `ExtractScalar { Int }`, arm A
  ldr   x0, [x23]                    ; the header's descriptor word
  movz  x1, #45944
  movk  x1, x1, #609, LSL #16        ; 0x1_0261_B378 = &scalars::INT, this run
  movk  x1, x1, #1,   LSL #32
  subs  xzr, x0, x1
  b.eq  label7 ; b label6
```

The particular address moves with the load address; the three instructions do
not, because any address in that range needs all three halves.

Six machine instructions to ask one question, three of them re-deriving a
number that is fixed for the life of the process. Handover 25 §3 reported "31
`movz`/`movk` pairs per iteration" of its sample loop and called it 14% of the
loop's machine instructions; the count reproduced here is **27** — nine proof
sites at three each — which is 12.6% of 215, so the figure was in the right
place and slightly high. §3 also closed the obvious objection in advance:
`opt_level = "speed"` does not remove them. It measured 216 instructions in the
loop at both settings, the *same* instructions, because a register allocator
rematerializes a constant on purpose rather than keeping it live across a loop.
Redundancy of this shape is the lowering's to remove, not the mid-end's.

**Handover 25 §3 says seven proof sites and the number is nine.** Handover 27 §9
flagged the discrepancy from a walk of `build.rs` and it is settled here by
census rather than by walking:
`the_sample_loop_proves_a_scalars_descriptor_nine_times_per_iteration` lowers
that exact program and counts eight `ExtractScalar{Int}` and one
`ExtractScalar{Bool}` in the loop region. The same census answers three
`CheckFault`s, which is what handover 25 §3 independently reports, so the
machinery agrees with it everywhere it is right. The acceptance criterion was
written as "7 sites × 2 instructions = 14 fewer per iteration"; it is eighteen.

## Decision 1: a by-value array in `RuntimeContext`, indexed by `BuiltinTypeId`

```rust
pub struct RuntimeContext {
    // …
    pub descriptors: [*const TypeDescriptor; BuiltinTypeId::COUNT],
}
```

and the proof becomes

```text
block5:
  ldr   x0, [x24]                    ; the header's descriptor word
  ldr   x1, [x23, #152]              ; ctx + descriptor_offset(Int)
  subs  xzr, x0, x1
  b.eq  label7 ; b label6
```

Three candidate shapes were on the table and the array is the only one that is
one load at a compile-time displacement.

**A base pointer — `descriptors: *const *const TypeDescriptor` — is rejected**,
and not because it is a word smaller. It is **two dependent loads**: the base,
then the slot, with the second unable to issue until the first retires. Trading
three independent ALU operations for two dependent loads is a worse trade than
the one this makes; trading them for one is the whole package.

`small_ints` is the base pointer this could have copied and it is a base pointer
for reasons that do not hold here. Its table is `SMALL_INT_COUNT` = 1281
entries, so by value it would be 10 KiB of context copied per
`Runtime::context`; and `Inst::Materialize { Int }` indexes it by a **runtime**
value, so the base has to be in a register before anything can be added to it
whatever the shape. This table is 22 entries — 176 bytes — and every index into
it is a compile-time constant, so folding the index into the displacement is
free and the second load is pure loss.

**Four plain fields — `int_descriptor`, `bool_descriptor`, … — are rejected**
because they would put a second `ScalarKind → which field` mapping in the
backend beside the one `inline_scalar_load_of` already is. That mapping is
already the crate's declared second statement of `ScalarKind::load_symbol`
(MIR-10) and it is exhaustive so a new variant fails the build; a second one
keyed on field name would be a third statement with no such guard. The array
collapses the question: the slot index *is* the descriptor's identity, so there
is one mapping and it answers both halves at once.

**The immediate, kept, is what this replaces**, and it is worth saying why it was
right in ADR-102 and is wrong now. ADR-102's site was a call before it; against a
`bl` and a full caller-saved clobber, three `movz`/`movk` did not signify.
Against the four-instruction sequence ADR-102 itself produced, they are half the
site.

## Decision 2: the compiler names a slot, and therefore holds no descriptor address

`inline_scalar_load_of` used to answer `&'static TypeDescriptor`. It now answers
`BuiltinTypeId`, and `emit_scalar_load` folds
`RuntimeContext::descriptor_offset(id)`. **This is the part that is a design
result rather than an optimization.**

Before, the emitted proof carried an address that the compiler read out of
*its own* `praxis-runtime` and baked into code the runtime would execute. Those
are the same `static` today only because the compiler and the runtime are one
binary — which §11.6's whole ABI-version apparatus exists because they will not
always be. A compiler and a runtime that disagreed about where `scalars::INT`
lives would emit a proof that fails for every correctly-typed `Int` and takes the
cold arm into `praxis_int_load`, which would then read the descriptor, find it
correct, and return — a program that is merely slow, until some later change
makes the cold arm a refusal instead.

After, there is no address in emitted code. The backend names a slot; the
runtime fills the slot; the two can only disagree about *which slot* `Int` is in,
and that is `BuiltinTypeId::Int as usize`, an enum discriminant with
`builtins_are_indexed_by_their_id` already standing over it.
`a_scalar_proof_loads_its_descriptor_from_the_context` asserts both halves: the
load is from the slot the id indexes, and **no built-in's address appears as an
`iconst` anywhere in the function** — the negative half, checked against every
one of the 22 rather than the one being proved, so a partial revert for a single
scalar cannot pass it.

`the_inline_check_proves_exactly_what_the_wrapper_would` is unchanged in force
and now reads the descriptor back through `id.descriptor()`. That is the same
`BUILTINS` registry `Runtime::context` fills the table from, so proving the
identity there proves it of the address generated code will load.

## Decision 3: the table is derived from `BUILTINS`, never written out

```rust
pub fn builtin_descriptor_addresses() -> [*const TypeDescriptor; BuiltinTypeId::COUNT] {
    BUILTINS.map(|d| d as *const TypeDescriptor)
}
```

A hand-written table would be a second statement of the index-to-descriptor
correspondence, and a slot holding a neighbour's descriptor is not a cosmetic
error: it is a proof of the wrong type, followed by a payload read at whatever
width the backend folded for the type it thought it had. That is REP-37 with the
table as its source, and it would be invisible to every test that does not
happen to use the mistyped scalar. Mapping `BUILTINS` leaves the registry as the
one place the correspondence is stated and
`builtins_are_indexed_by_their_id` as the one gate on it.

`every_descriptor_slot_holds_the_builtin_whose_id_indexes_it` checks it anyway,
and checks it **at the byte offset generated code reads** rather than by
indexing the Rust array. Indexing would re-derive the stride from `size_of` and
so would agree with `descriptor_offset` even if both were wrong in the same way;
reading `base.add(descriptor_offset(id))` out of a live context is the question
the JIT asks.

A `fn` and not a `const fn`, which is forced: const evaluation may not read a
`static`, and `BUILTINS` is a `static` deliberately — the addresses *are* the
identities (`builtin_descriptors_have_a_stable_address`). It is called once per
`Runtime::context`, and the cost of that is priced below.

## Decision 4: a placeholder context carries the real table

`RuntimeContext::placeholder` nulls every pointer it can, with a documented
reason each time: a null faults loudly at the first read rather than aliasing
whatever the `input_source` trick would have pointed at. **The descriptors are
the one field where that argument does not apply**, and they are filled.

Every other pointer in that struct is something a `Runtime` has to wire. These
are addresses of `static`s of this binary: valid before `main`, dependent on no
heap, no fault slot and no runtime at all. Nulling them would be a trap for a
state that cannot arise, and it would change what a placeholder does at
ADR-102's proof site from *compare unequal and take the cold arm* to *segfault on
the load* — strictly less informative, in the one constructor whose whole
purpose is test scaffolding.
`a_placeholder_context_still_knows_every_builtin_descriptor` is the gate.

## `RUNTIME_ABI_VERSION` 19 → 20, and the three lines that were pre-written

**Four packages in this round have a claim on that constant and exactly one may
take it** (handover 26 §5, §7 trap 5). Four bumps in four worktrees is not a
merge conflict; it is one merge that silently keeps whichever it saw last, and
the losing packages' changelog paragraphs go with it. W6 takes the numeral. W4b,
W10 and W8-S0b each have an **owned placeholder line** under the v20 heading
carrying the same *owed* status word `docs/decisions/README.md` uses for a
reserved ADR number, so appending their paragraph is a one-line edit at a line
nobody else touches rather than an append two branches both perform at the same
offset.

Two of the three historical rules for a bump fire here, which is unusual:

- **the struct grew**, by `22 × 8` bytes, which is the v9 (`native_roots`) and
  v13 (`fault_message`) rule — a host that built a v19-sized context and handed
  it to this runtime is 176 bytes short of the table;
- **generated code reads the new field**, which is v15's rule.

The direction worth writing down is the second one, because it is the direction
the mismatch is *quiet* in. A v20-compiled program against a v19 runtime reads
176 bytes past the end of a context nobody sized for a table. If that read is
mapped — and against a `RuntimeContext` on a host's stack or in a `Box` it very
likely is — it compares the header's descriptor against whatever was there,
fails the proof for a correctly-typed value, takes the cold arm, and
`praxis_int_load` re-reads the descriptor, finds it right, and returns the
payload. **The program is correct and slow**, which is the worst kind of ABI
mismatch to have to diagnose. That is exactly the failure mode Decision 2
removes for the two sides' *own* addresses and cannot remove for the struct's
size, which is what the version number is for.

The three satellite readers handover 26 §5 names were each visited:

- `abi.rs`'s `version_is_nineteen_for_the_batch_this_build_ships` is renamed and
  re-asserted at 20. Its doc is rewritten to say that a version gathers changes
  in general rather than to list v19's four, which had made the test read as
  being about v19.
- `gc.rs`'s `the_header_shrink_moved_the_folded_payload_offset_at_abi_v19` is
  the interesting one. It pinned the folded payload offset *beside* the version
  so the two could only be updated together, and its own comment asks that "a
  later bump must not orphan this test" — but as written any bump failed it.
  Renamed to `the_folded_payload_offset_moved_at_v19_and_is_pinned_here` and
  re-asserted at 20: the offset moved *at* v19 and has not moved since, and what
  the assertion buys is that whoever bumps the version comes through here and
  re-confirms the immediate. Pinning it to 19 forever would have made the next
  bump a mechanical edit of a failing number, which is the same thing as
  deleting the test.
- `lib.rs:55` is the `pub use` re-export and carries no numeral.

`COMPILER_EXPECTED_ABI_VERSION` moves with it; `assert_passes_within_a_single_build`
is what would catch it if it had not.

## The test helper that had to be repaired first, and the shape of its failure

Handover 26 §4 predicted this and it is worth recording that the prediction was
exact, because the mechanism generalizes.

`payload_load` picks the one instruction reading at the payload displacement,
and it did so with `line.contains("+16")`. Cranelift prints an address as
`vN+DISP`, and **`"v0+168".contains("+16")` is true**. The table lands at offset
136, so `Char`'s slot is at 136 + 4×8 = **168**, and the descriptor load in a
`Char` proof collides with the payload displacement while `Int`'s (152) and
`Float`'s (176) do not. The presentation is the worst available: exactly one of
three sub-cases of `a_bool_extract_reads_one_byte_and_a_char_four` — a test
about payload *widths* — fails, with the message "more than one instruction
reads at +16". Reproduced deliberately here before the fix, to confirm the
diagnosis rather than infer it.

The repair is to match the displacement as a whole address token: split on
whitespace and ask which token *ends* with `+16`. That is exact rather than
merely better, because `+` occurs once in a `vN+DISP` token, so no longer
displacement can end with a shorter one.

## Measurements

**Build phase, so nothing is timed.** Handover 26 §6's rule is that where a
deterministic count exists it is the honest headline, and this is a package
where one does. Two agents were compiling beside this one throughout; any clock
reading taken here would be discarded.

Wave 0's `PRAXIS_DUMP_CLIF` / `PRAXIS_DUMP_VCODE` over handover 25 §3's loop, at
the release build, per-iteration counts read by `dump.rs`'s documented rule (the
one multi-block strongly connected component; from its header take, at each
branch, the successor inside the component that is not cold):

| | arm A (toggle reverted) | arm B (this branch) | |
|---|---:|---:|---|
| CLIF, whole function | 311 in 55 blocks | 311 in 55 blocks | — |
| CLIF, per iteration | 171 over 35 blocks | 171 over 35 blocks | — |
| vcode, whole function | 458 in 67 blocks | **439** | **−19** |
| vcode, per iteration | 215 over 38 blocks | **197** | **−18** |
| machine code | 1960 bytes | **1884 bytes** | −76 |

**Nine sites × two instructions = eighteen fewer per iteration, exactly.** The
CLIF is unchanged to the instruction because an `iconst` became a `load`, one
for one — which is also the cleanest available demonstration that the win is not
visible at the IR level and that a package stating its headline in CLIF would
have reported zero.

The mnemonic histogram says the same thing a second way, whole-function:

| | arm A | arm B |
|---|---:|---:|
| `movz` | 19 | 10 |
| `movk` | 20 | 2 |
| `ldr` | 69 | 78 |
| `mov` | 87 | 86 |

Twenty-seven address halves removed, nine loads added: −18. The nineteenth
instruction is a register-to-register `mov` the allocator no longer needs, which
is a bonus and is not claimed as part of the mechanism.

**Arm A reproduces `1535eb6`'s recorded baseline exactly** — 311 CLIF, 458
vcode, 1960 bytes, against the 311/458/1960 in `dump.rs`'s module doc. That is
the strongest available evidence that the toggle is the whole change and that
nothing else on this branch moved generated code.

And the emitted code closes the loop on the site count independently of the
census. Arm B's vcode contains **nine** slot loads and no others: eight
`ldr xN, [x23, #152]` and one `ldr xN, [x23, #144]`, which are
`descriptor_offset(Int)` and `descriptor_offset(Bool)` — the same eight-and-one
split the MIR census reports, arrived at from the other end of the compiler.
All nine go through one base register: the context is not reloaded anywhere in
the loop.

### The arms

Handover 26 §6: the baseline arm is this tree with the package's single toggle
point reverted, never the previous commit. ADR-113 records that mistake giving
−14.4% where the truth was −0.8%.

The toggle is the `adr116-arm-a` cargo feature on
`praxis-codegen-cranelift`, and it is two lines in `emit_scalar_load`: with it
on, `want` is an `iconst` of `builtin.descriptor()`'s address again. The context
still carries the table, the ABI version is still 20, and every other file is
byte-for-byte identical, so the comparison is of the proof's shape and of
nothing else.

    cargo build --release -p praxis-cli                                # arm B
    cargo build --release -p praxis-cli \
        --features praxis-codegen-cranelift/adr116-arm-a               # arm A

| | sha256 |
|---|---|
| `/tmp/praxis-arms/W6-a` | `d5f1d02449bbcc84ee48ea243b96dfb733f14badbafec7dcc88448f67a1b1907` |
| `/tmp/praxis-arms/W6-b` | `2bdcdb99c7438bac5c5da7aacedddf2432fe3ffb5a14e3cc811e2d684cd4bda1` |

**The toggle is verified to bite from both directions.**
`a_scalar_proof_loads_its_descriptor_from_the_context` is arm-B-only, because
under the feature the emitted code holds exactly the address it asserts is
absent — a feature that did nothing would leave it green in both arms and the
A/B would be comparing a binary with itself. The two hashes differ. Both arms
pass their crate's whole test suite.

All eight benchmarks were run once at frozen `sizes.json` sizes under both arms
and printed **byte-identical stdout**. That is a correctness check, not a
measurement, and no wall-clock number was taken.

### What the clock may not be able to say, stated before anyone measures

**This trades three independent ALU operations for one L1 load-use dependency,
and on an M2 Pro that is not obviously a win in time even though it is
unambiguously a win in instructions.** Handover 26 §9 registered it as
unverifiable by reading and it still is. The reasons to expect the trade to be
favourable are that the context line is already resident — the prologue's
recursion guard, `Inst::ConstGc`'s `small_ints` base and every `CheckFault` read
through the same base register — and that the load's result is consumed by a
compare whose branch is perfectly predicted, so the dependency is on the
not-taken path of a branch the front end has already resolved. The reason to
doubt it is that a `movz`/`movk` triple has no latency the scheduler cannot
hide either.

**So the instruction count is this ADR's result, and the wave-2 measurement
phase should report the clock's answer whatever it is, including "cannot
resolve".** A sub-2% single-benchmark delta on this laptop is not a result
(handover 26 §6), and eighteen instructions out of 215 on a loop that is 4%
arithmetic will very likely land under that floor. Reporting it as a
1-point-something-percent win would be the kind of claim handover 21 §3.6
records being made once already.

## What was deliberately *not* done

**The other descriptor immediates are left alone.** `lower.rs` emits
`iconst el_desc` in the `Inst::Alloc` arm for a collection's element descriptor,
and those are not moved into the table. Two reasons: they are *arguments to a
call that is happening anyway*, so the address has to reach a register
regardless and the immediate costs nothing a load would not; and
`collection_element_descriptor_for` can answer a **generated** descriptor for a
tuple or record shape, which is not a built-in and has no slot. Narrowing the
change to the proof site keeps the claim "the backend holds no descriptor
address" true of exactly the place it matters and false nowhere silently — the
IR-shape test asserts it of `emit_scalar_load`'s output, which is where it
holds.

**The table is not per-`Runtime`.** `Runtime::context` fills every context from
the same `BUILTINS`, and `two_runtimes_agree_on_every_descriptor_address` pins
it. A future in which descriptors were minted per runtime would make code
compiled against one unusable against another; that is a much larger decision
than a field, and this is where it would be noticed.

**`Inst::EnumTag` still reads a payload with no descriptor check at all.**
ADR-102 left that open and this does not close it. It does make it cheaper to
close: the objection was never the compare, it was that the cold arm needs a
callee, and `praxis_enum_tag`'s `Effect::Allocates` row would make the site a
nominal safepoint. Nothing here bears on that.

**No `Safepoint` obligation arises and none is discharged.** The table holds
immortal `static`s; reading a slot allocates nothing, can fault nothing, and
observes no heap state. ADR-113's decision 1 argument is untouched — this is not
an allocation fast path and it forges no token, because it never asks for one.

## Consequences

- **The proof site is now four machine instructions and one of them is a
  branch.** Anything that wants to make a scalar read cheaper from here has to
  remove the site, not shrink it. That is W11's backend half and nothing else.
- **W11 does not stack on top of this, and the overlap is not symmetric.**
  `emit_scalar_load` is the language's only descriptor-proof emitter and its
  sole non-test caller is `lower_inst`'s `ExtractScalar` arm, so at any site W11
  elides, this contributes exactly **zero** — the load introduced here
  disappears along with the `icmp` it feeds. In the other direction W11's
  residual per elided site drops from six instructions to four, because two of
  the six were the `movz`/`movk` pair this already removed. **The double count is
  two instructions per site, in W11's favour, and W11's own accounting must be
  written against this tree.** Handover 26 §4 read the overlap as symmetric;
  handover 27 §5 corrected it and this is that correction, confirmed against
  emitted code.
- **W10 was never blocked on this**, and handover 26 §3's "both consume W6's
  descriptor table" is false for it (handover 27 §4). `lower.rs` bakes
  descriptor addresses as `iconst`s at header-store sites too, so W10 is a
  two-instruction-per-store *discount* if it lands and nothing at all if it does
  not. It should not schedule itself behind this.
- **`RuntimeContext` is 312 bytes, from 136.** `Runtime::context` returns it by
  value, so the cost is one 176-byte block move per call — and every call site is
  once per program run, per debugger reload, or per `collect_now`, which is a
  host entry point that then performs a full mark-and-sweep. Nothing on the path
  from generated code into the runtime constructs a context; the wrappers take a
  `*mut RuntimeContext`. If a future change puts `Runtime::context` on a hot
  path, this is the field that makes it expensive, and the answer then is the
  base pointer Decision 1 rejects — the trade would have reversed.
- **Repacking `RuntimeContext` is now a generated-code change in one more
  place.** It already was, for `shadow`, `debug_frames`, `debug_values`,
  `pending_fault`, `small_ints`, `stack_left`, `heap` and the two `Bool`
  immortals. The table is the first *array* among them, so the thing that must
  not change is not only the field's position but the element stride;
  `descriptor_offset` is the one authority for both and the backend calls it
  rather than multiplying.
- **A twenty-third built-in costs eight bytes of context and nothing else.**
  `BuiltinTypeId::COUNT` sizes the array, `BUILTINS`'s length is checked against
  it, and the second `const _` beside the field asserts the last slot is the
  last word of the struct — so adding a variant without adding an entry is
  already a build error and adding both is a build success with a correct table.
- **The new memory-safety surface is one in-bounds read, and it is bounded by a
  `const _` rather than by a sanitizer.** Handover 26 §7 item 6 is right that
  ASan does not instrument JIT-generated code, so a green ASan run would say
  nothing about the load this adds. What does say something is that the
  displacement is `RuntimeContext::descriptor_offset` of a `BuiltinTypeId`, that
  the second `const _` beside the field asserts the last slot ends at
  `size_of::<RuntimeContext>()`, and that the only way to reach the function is
  with an id — so an out-of-bounds displacement has no spelling. That is a
  weaker claim than W4b's and W10's will need and a much smaller one.
- **The test-helper repair generalizes.** Three other IR-shape assertions in
  `lower.rs` match displacements by substring
  (`an_inline_int_box_tests_the_pacing_counter_before_it_reads_the_table` and
  its neighbours, against `Heap` field offsets). They are not wrong today and are
  left alone deliberately — they are the wave's blast radius otherwise — but the
  next package that appends a field near one of those offsets will meet the same
  failure, and the fix is the one written here.

## Open questions

- **Is it a wall-clock win?** Unresolved by construction: the build phase cannot
  answer it and the instruction count does not. The wave-2 measurement phase
  should run the arms and report what it finds, including that the difference is
  below the noise floor if that is what it is. The benchmark most likely to show
  it is `collatz` or `primes`, which are the two the sample loop is
  representative of; `bfs` and `vm` are dominated by runtime calls and should not
  move.
- **Should the four wired scalars' slots be adjacent?** `Bool`, `Int`, `Char`
  and `Float` are at 144, 152, 168 and 176, with unwired `Byte` at 160 between
  `Int` and `Char`. That span is inside one 64-byte line already, so packing
  them buys nothing — and it would mean reordering `BuiltinTypeId`, whose
  discriminant *is* a descriptor's `TypeId` and therefore runtime type identity
  (ADR-038). Recorded so the next person does not have to work out why not.
- **Should the context pointer be pinned to a register?** Every proof reloads
  nothing — `x23` held the context across the whole loop in the dump — but that
  is the allocator's choice at `opt_level = "none"` and not a guarantee. If a
  function with more live values spills it, each proof becomes two loads and
  this package's win goes to zero there. Nothing in the tree measures that today;
  the sample loop is the only program whose emitted code anyone has counted.

---

## Amendment, 2026-08-03 — the denominator was nine and is five

**Amended by [ADR-120 part 2](./120-a-box-with-one-reader-in-its-own-block-is-not-a-box.md),
which carries the measurement.** ADR-120's block-local box/unbox forwarding
landed in the same wave as this package and deletes four of the nine proof sites
in handover 25 §3's loop — three interior nodes of the expression trees, and the
`while` condition's whole `Materialize{Bool}` → `ExtractScalar{Bool}` round
trip. This record's headline arithmetic, "**nine sites × two instructions =
eighteen fewer per iteration, exactly**", was exact when it was written and is
stated against a tree that no longer exists.

Re-measured on the merged tree with `adr116-arm-a` as the only toggle reverted,
by the same `PRAXIS_DUMP_VCODE` rule: **125 → 115 machine instructions per
iteration, −10.** Five sites × two, exactly, again.

The mechanism, the `RUNTIME_ABI_VERSION` bump, the decisions and the
whole-program figures below are unchanged and unaffected; what moved is the
count of sites the loop contains, and therefore the number a later package
re-pricing the descriptor table must net against. The per-program table in
"Measurement" was taken before the forwarding landed and should be read the same
way. Both tests this record points at as pins are renamed and now assert five,
each carrying the table of all three answers and why they differ:
`the_sample_loop_proves_nine_descriptors_per_iteration_not_seven` (`lower.rs`)
is `the_sample_loop_proves_five_descriptors_per_iteration_where_nine_were_written`,
and `the_sample_loop_proves_a_scalars_descriptor_nine_times_per_iteration`
(`mir_shape.rs`) is
`the_sample_loop_proves_a_scalars_descriptor_five_times_per_iteration`.

This is the double count handover 21 §3.6 recorded and handover 26 §7 trap 7
warned about. It arrived as a failing test at the wave merge rather than as a
wrong number in a report, which is what the wave structure is for.

---

## Amendment, 2026-08-04 — both figures were re-derived from rebuilt trees, and both stand

Two different per-iteration walkers were in use this round and both were wrong:
the shared helper ADR-118 part 2 caught, which read the vcode's per-block counts
through the *CLIF* control-flow graph, and a later one that built each graph
correctly but had no set of cold vcode blocks. So this record's two figures — the
original **−18** and the amendment above's **−10** — were re-derived rather than
assumed.

The walk is now `benchmarks/periter.py`, which walks each IR over its own graph
and self-tests; `dump.rs`'s module doc says where the rule needed the
qualification. It reproduces Wave 0's recorded baseline exactly — 311 CLIF in 55
blocks and 171 over 35, 458 vcode in 67 blocks + prologue and 215 over 38 — which
is the strongest available check, because that pair was recorded before any of
these walkers existed and its two denominators differ.

**The original −18: right when written.** This record's own tree (`fa15c59`) was
rebuilt, both arms:

| | arm A | arm B | delta |
|---|---:|---:|---:|
| CLIF, per iteration | 171 over 35 blocks | 171 over 35 blocks | — |
| vcode, whole function | 458 in 67 blocks, 1960 bytes | 439 in 67, 1884 bytes | −19 |
| vcode, per iteration | 215 over 38 blocks | **197** over 38 blocks | **−18** |

Every figure in "Measurements" above reproduces to the instruction. Nine sites ×
two is eighteen, and it was eighteen.

**The amendment's −10: right too.** At `2491140`, `adr116-arm-a` the only toggle
reverted: **125 → 115** machine instructions per iteration, 21 blocks either
way, and 106 → 106 CLIF. Five sites × two.

So nothing in this record is corrected, and the reason for saying so at length is
that a reader of this round now has to tell a figure that was checked from one
that was not. ADR-117's amendment and two cells of ADR-120's part-2 table did
not survive the same check; this record's did.
