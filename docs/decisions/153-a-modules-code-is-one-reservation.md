# ADR-153: A module's code is one reservation, and a host that veneers a far call is not a witness

**Date:** 2026-08-17
**Status:** Accepted — implemented.
**Milestone:** post-M11 backend correctness
**Amends:** nothing structurally. It writes down a constraint the backend has
always had and never stated: the encodings Cranelift picks for a cross-function
reference are only legal if the code is placed to make them legal. ADR-027's
closures and ADR-029's fused pipelines are the constructs that mint the most of
those references, and neither record says where the code lands.

## Context

### The complaint

The nightly `asan` job (ADR-002's second workflow) fails on the same commit on
roughly every other run. Three failure sites, all in the same shape:

```
thread 'pipeline_three_stage_map_filter_map_sum' panicked at
  cranelift-jit-0.134.2/src/compiled_blob.rs:142:80:
called `Result::unwrap()` on an `Err` value: TryFromIntError(PosOverflow)
```

`adv_pipeline_collect_vec_elements_survive_gc_stress` fails the same way in the
same binary, and `every_corpus_program_runs_and_prints_the_answer_it_documents`
fails because the child `praxis` it spawns for
`adr127_pipeline_over_every_iterable.px` dies of it with status 101.

**It is not a sanitizer report.** ASan found nothing; the process panicked
before ASan had an opinion. `scripts/asan.sh` says in its own header that the
sanitizer is blind to generated code, and this is the adjacent case it does not
cover: a panic in the code that *installs* generated code.

### What the panic is

`compiled_blob.rs:142` is the whole of cranelift-jit's x86-64 PC-relative
relocation handling:

```rust
Reloc::X86PCRel4 | Reloc::X86CallPCRel4 => {
    let what = relocation_target_addr(name, addend);
    let pcrel = i32::try_from((what as isize) - (at as isize)).unwrap();
    unsafe { write_unaligned(at as *mut i32, pcrel) };
}
```

The distance between two pieces of generated code did not fit in a signed 32-bit
displacement. `PosOverflow` rather than `NegOverflow` says the target sat
*above* the reference, which is the direction Linux's top-down `mmap` produces
for functions defined in declaration order.

### The chain that gets there

1. `Jit::compile` declares every user function `Linkage::Export`.
2. `Linkage::is_final()` is true for `Export`, so `declare_func_in_func` marks
   every cross-function reference **colocated**.
3. cranelift-jit forces `is_pic=false` (`backend.rs:69`, and it asserts the flag
   at `JITModule::new`). With `!is_pic` and a colocated target the x64 backend
   picks its short encodings, and says why in its own comment at
   `cranelift-codegen/src/isa/x64/inst/emit.rs:1419` — *"If we know the distance
   to the name is within 2GB (e.g., a module-local function)"*:
   - a call becomes `call rel32` (`emit.rs:423`), from `user_funcref`
   - a `func_addr` becomes `lea (%rip)` (`emit.rs:1424`), from `AllocKind::Closure`
   
   Both are `Reloc::X86CallPCRel4`. The `praxis_*` runtime imports are unaffected:
   `Linkage::Import` is not final, so those get an absolute `Abs8`.
4. Nothing establishes the 2GiB. Cranelift-jit's default `SystemMemoryProvider`
   takes a fresh `MmapMut::map_anon` per code chunk and never over-allocates —
   `memory/system.rs:88` carries a `// TODO: Allocate more at a time` — so the
   spacing between chunks is the kernel's choice and no more.

**"Module-local" is an assumption the code generator makes and the memory
allocator does not honour.** It is adjacent often enough to look like a
guarantee.

### How many chunks that is

`PRAXIS_DUMP_VCODE=all` over the corpus:

| program | functions | code bytes |
| --- | ---: | ---: |
| `adr127_pipeline_over_every_iterable.px` | 17 | 54,948 |
| `aoc2025_day12.px` | 17 | 45,280 |
| `aoc2025_day09.px` | 8 | 58,508 |

Roughly 3.2 KB per function against the runners' 4 KiB pages: a function fills
its page and leaves too little for the next one, so a 17-function program is
about 17 independent placement decisions that all have to land mutually in
range. **This laptop's pages are 16 KiB**, so it packs about four functions per
chunk where the runner packs one — the local shape is not the CI shape even
before the sanitizer is involved.

That the three failures are all pipelines is not luck either. Every stage of a
fused chain (ADR-029) is a closure lowered to its own synthetic function
(ADR-027), so a pipeline is the program shape that mints the most functions and
therefore the most references that must reach.

### Why ASan, and why intermittently

**This part is inferred and is not measured on the runner.** ASan does not cause
the overflow; it perturbs the address space until the assumption breaks — large
fixed shadow reservations, plus its own allocator's `mmap`/`munmap` traffic
interleaving with the JIT's chunks. ASLR is why the layout differs per run and
the job fails about every other night rather than always. The `PosOverflow`
direction is consistent with that model and is the only part of it this record
can point at as evidence.

What *is* established is that the sanitizer is incidental: any x86-64 host whose
address space is fragmented enough can hit this running an ordinary program.
This is a latent bug in the shipped compiler that a nightly job happened to find.

### Why the laptop cannot see it

Not "a different address space" — a structural difference in the same file. The
arm directly below the panicking one handles `Reloc::Arm64Call` by **rewriting a
too-far call into a veneer**: `CompiledBlob::new` pre-reserves 24 bytes per call
against exactly this case, and the relocation pass writes an `ldr x16` / `br x16`
stub and points the call at it. The x86-64 arm has no such fallback.

So a violation of this invariant is silently repaired on aarch64 and fatal on
x86-64. A test that asserts the generated code *runs* can therefore never fail
on this host, however wrong the placement is. (The closure half is not fully
repaired even here: `Reloc::Aarch64AdrPrelPgHi21` has the same unchecked
`i32::try_from`, under a comment conceding the field is really i33 and that the
target is "unlikely" to be that far.)

### Reproduced, on this machine

Rosetta 2 runs x86-64 on an Apple Silicon host, which is enough to exercise the
relocation path the runners take. A standalone harness against cranelift-jit
0.134.2 — two `Linkage::Export` functions, one referencing the other, with a
deliberate anonymous `map_anon` opened between the two `define_function` calls —
reproduces the CI panic at the same file, line **and column**, for both the call
and the closure encoding:

| arm | callee − caller | result |
| --- | ---: | --- |
| no hole | 12,288 B | runs |
| 3 GiB hole, `call` | — | **panic, `142:80`, `PosOverflow`** |
| 3 GiB hole, `func_addr` | — | **panic, `142:80`, `PosOverflow`** |
| 3 GiB hole, arena | 10,892 B | runs |
| 3 GiB hole, `Preemptible` | 3,221,237,760 B | runs |

The same harness built for aarch64 cannot be made to fail: the forced hole lands
in a different region of the address space entirely and the chunks stay adjacent.
That is the point of the row rather than a gap in it.

## Decision 1: one `Jit`'s code comes out of one reservation

`Jit::in_generation` installs an `ArenaMemoryProvider` sized
`CODE_RESERVATION_BYTES`, so every chunk — executable, read-only and writable —
is carved from a single contiguous region reserved at construction. Both
constructors and every host funnel through this one site: `Jit::new`, the CLI's
`run`, the debugger's session, and the debugger's per-`p EXPR` module.

This does not change what Cranelift emits. It makes the encoding Cranelift
already chose legal, which is the only available move: an out-of-range target is
a **panic inside the relocation pass**, not a `ModuleError` this crate could
catch, so the range can be established but never recovered from.

**The size is 64 MiB**, a ceiling on one `Jit`'s total generated code. Against
the corpus table above that is ~1100× headroom, and exhausting it fails
compilation — the `ModuleError::Allocation` leaves `define_function` and arrives
as a `JitError::Cranelift` — rather than miscompiling.

It costs address space and not memory. The region is reserved `PROT_NONE` and
pages are committed as segments are handed out, so the resident cost stays the
size of the code. That distinction is load-bearing for the debugger, which mints
a `Jit` per `p EXPR` and whose finalized reservations are deliberately leaked
because the code stays callable: a long session leaks this constant per
expression *in address space*, which is affordable precisely because it is not
memory.

## Decision 2: failing to reserve is its own error

`JitError::CodeReservation` carries the reservation failure from
`ArenaMemoryProvider::new_with_size`. It is not `UnsupportedTarget` — the host is
supported, the address space was not available — and a reader who sees one should
be looking at the process's mappings rather than at Cranelift's ISA support.

## Decision 3: the invariant is asserted on addresses, not on behaviour

`every_generated_function_is_within_reach_of_every_other` compiles a three-stage
pipeline, collects every entry pointer, and asserts the span between the extremes
fits in an `i32`.

**It asserts the addresses precisely because a behavioural test cannot fail
here.** aarch64 veneers a too-far call, so "the pipeline computes 36" stays green
under an arbitrarily bad placement, and the only gate the change has at the
keyboard would be one that cannot observe what it is guarding. Asserting the
property directly states it on every host, and fails on the host where violating
it is fatal.

## Decision 4: `Linkage::Preemptible` is **rejected**

The one-word alternative: `Preemptible` is not a final linkage, so references
stop being colocated and both encodings demote to an absolute `movabsq` with an
`Abs8` relocation, which has no range limit at all. It is measured in the table
above and it works — at 3 GiB apart, which is the strongest available proof that
colocation is the trigger and not a side effect.

It is rejected because it prices every call between user functions at an
indirect jump through a register instead of a direct `call rel32`, permanently
and on every host, to buy what a reservation buys for free. `CRANELIFT_FLAGS`
records a suite geometric mean of **1.025×** for `opt_level`; a change in the
opposite direction of that magnitude is not worth taking when the alternative
costs address space and nothing else. Recorded so it is not re-derived.

## Decision 5: bumping the dependency is **not** a fix

Checked, because it is the cheapest thing that could have worked:
`cranelift-jit` 0.134.3's `compiled_blob.rs` is **byte-identical** to 0.134.2's.
There is no version to move to.

## Verification

Native aarch64: `cargo test --workspace` green, `cargo fmt --check` clean,
`cargo clippy --workspace --all-targets -- -D warnings` clean.

The runs that carry the weight are the ones through the failing relocation path,
built for `x86_64-apple-darwin` and run under Rosetta 2 with the fix in place:

| suite | result |
| --- | --- |
| `praxis-codegen-cranelift --test jit` | 537 passed, 0 failed |
| `pipeline_three_stage_map_filter_map_sum` | passes |
| `adv_pipeline_collect_vec_elements_survive_gc_stress` | passes |
| `praxis-cli --test corpus` | passes |

All three CI failure sites, executing the `X86CallPCRel4` path that was breaking.

## Consequences

**What this does not change.** Not one byte of generated code differs: the
linkage, the flags, the encodings and the ABI are what they were, and
`RUNTIME_ABI_VERSION` does not move. This changes where the code is put, and
nothing about what it is.

**What it owes.**

1. The nightly `asan` job is the only gate that can observe this, and one green
   run does not clear it — the failure was intermittent to begin with. A week of
   green nights is the evidence, and until then this record's ASan mechanism
   stays inferred.
2. The missing x86-64 veneer is worth reporting upstream. cranelift-jit already
   solves this for aarch64 in the same function; nothing about the x86-64 case
   makes it harder, and a fix there would make the invariant unnecessary rather
   than merely held.
3. `CODE_RESERVATION_BYTES` is one constant serving both a whole-program compile
   and the debugger's throwaway per-expression modules. If a session's leaked
   address space ever becomes a real number, the debugger's path wants its own
   smaller reservation rather than a smaller constant for everyone.

**The standing constraint.** Two properties now hold this up and neither is
local to the file that states it: user functions are declared with a *final*
linkage, and generated code lives in *one contiguous reservation*. A change to
either re-arms the panic. The first is in `Jit::compile`, the second in
`Jit::in_generation`, and the host most likely to be running the tests when
either changes is the one that repairs the damage silently.
