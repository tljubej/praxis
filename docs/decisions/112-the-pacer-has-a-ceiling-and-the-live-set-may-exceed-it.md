# ADR-112: The pacer has a ceiling, and only the live set may exceed it

**Date:** 2026-08-03
**Status:** accepted — implemented
**Milestone:** post-M11 performance (handover 23 item P-2)
**Amends:** ADR-011's pacing heuristic. `collect_threshold` still doubles, but
the doubling is now the *speculative* half of a two-term rule and it stops at a
ceiling. RT-04 ("only allocation pressure grows the threshold") is untouched and
gains a second reading: an immortal exerts no pressure and now also buys no
headroom. ADR-103's sweep is used as delivered — its "sweep does not touch
survivors" property is what makes the live measurement free, and it is preserved
exactly. ADR-106's `collect_inner` ordering is unchanged; the re-pacing that was
already the last step is still the last step.

## Context

`Heap::collect_inner` set `collect_threshold = max(previous × 2, INITIAL)` after
every paced collection, and nothing lowered it except `Heap::reset`. So a
program's peak resident set was a function of **how long it had run** rather
than of how much it was holding: starting at 64 KiB, the *n*-th collection ran
after 64 KiB · 2ⁿ of fresh allocation, and the high-water mark before it was
that same figure. The benchmark suite peaked between 0.5 and 3.2 GiB while
holding, in six of eight cases, essentially nothing. `primes` peaked at 967 MiB
holding *no live data at all*.

That is a scalability defect and not a speed one, and it was already written
down twice — as `benchmarks/REPORT.md`'s "what would move these numbers" item 2
and as handover 23's P-2. Both said the same thing: what is wrong with the rule
is not its rate but that nothing bounds it.

**The reason it had not been fixed is that the one measurement that existed said
the fix was a loss.** An experiment at `fd70374` paced purely off the live set
(`live × 2`, floored at 8 MiB), cut peak memory by a large factor, and cost
**14% more time**. That number is preserved as
`benchmarks/gcfix-pre-perf-fixes.json`, and it is why handover 23 says to re-run
before designing anything.

It no longer applies, and the reason is mechanical rather than a matter of
opinion. At `fd70374` the free list was a `RefCell<HashMap<BlockLayout, Vec<…>>>`
with the default SipHash hasher, probed once per allocation and once per swept
block, over a `bumpalo::Bump` that never reused anything. Under the doubling
pacer collections were rare, so nearly every allocation was a *fresh* bump and
paid none of that. Pacing off the live set made nearly every allocation a
*reused* one — so the experiment did not measure the cost of collecting more
often, it measured the cost of the free list, and it charged that cost to the
pacer. ADR-103 deleted the free list. Reuse is now `PageHeader::claim_free_block`:
the same six instructions whether the block is fresh or recycled. The premise of
the −14% is gone.

There is a second reason the trade should have inverted, and it is the one the
measurements below bear out. A doubling pacer's 0.5–3.2 GiB peak is 0.5–3.2 GiB
of pages the kernel must zero on first touch. A bounded heap touches its working
set once and recycles it.

## Decision 1: the next paced threshold is `max(min(previous × 2, ceiling), live × k, INITIAL)`

Three terms, and each is there for its own reason.

* `min(previous × 2, ceiling)` — the **ratchet**. ADR-011's geometric growth,
  which is what keeps allocations per collection amortized O(1), retained
  verbatim up to a bound. `Pacer::Doubling` is this term without the `min`, and
  it remains in the tree as a named, tested statement of the rule this amends.
* `live × k` — the **mandatory headroom**, where `live` is the block bytes the
  sweep that just ran measured and `k` is `LIVE_HEADROOM`, 2. A program holding
  *L* bytes may allocate another *L* before the next collection, so the
  collector's marginal mark cost is capped at one mark of the live set per equal
  quantity of fresh allocation.
* `INITIAL` — the floor, unchanged, so a heap that has just collected everything
  does not then collect again on its next allocation.

`Pacer` is a value fixed at `Heap` construction, not a `Cell`. A collector that
could change its own schedule mid-run would make "when does this program
collect" a function of history rather than of the heap it was built with, and
every pacing test order-dependent.

**No separate growth-ratio floor is needed, and that is worth stating because
the `fd70374` experiment needed one.** The rule is monotonically non-decreasing
up to the ceiling: once `previous >= ceiling`, `min(previous × 2, ceiling)` is
`ceiling` forever. So the ratchet-to-ceiling *is* the floor, and it is what that
experiment's hand-picked 8 MiB was standing in for. The failure the floor
defended against — "collecting constantly against a small live set" — requires
`live × k` to be the *only* term. Here it is one of three.
`a_shrinking_live_set_does_not_lower_the_threshold_below_the_ceiling` is that
difference written as a test: a program that briefly holds a large live set and
then drops it settles back to the ceiling, not to 64 KiB.

## Decision 2: the ceiling clamps the ratchet term, never the whole expression

`min(ceiling)` applied to the *result* is a one-character edit, it looks like a
simplification, and it is a thrash bug. A program whose live set legitimately
exceeds the ceiling would get a threshold no larger than that live set, so every
collection would prove it can reclaim nothing and the next allocation would
trigger another. The collector would run at 100% of the program.

The distinction between the two terms is what makes the ceiling safe:

* the ratchet is a **guess about the future** — "this program has been
  allocating, so let it allocate more before we look again". A guess may be
  capped, because being wrong about it costs only a collection that finds
  nothing.
* `live × k` is a **statement about the present** — this many bytes are provably
  reachable *right now*, and no threshold below them can be met without the
  collector running against a heap it cannot shrink. It must be allowed to
  exceed the ceiling.

So the ceiling bounds speculative growth and nothing else, and the resident set
it delivers is `live + max(ceiling, live × k)` — a function of what the program
holds plus a constant, which is exactly the property P-2 exists to buy.

`a_bounded_pacer_gives_a_large_live_set_its_headroom` roots a live set of twice
the ceiling and asserts the threshold comes out at `live × k`. It is the test
that fails when someone folds the `min` over the `max`, and it exists because
without it every other test in this ADR still passes.

## Decision 3: `live` is block bytes, measured in the sweep, and the under-count is safe

`Heap::sweep` accumulates `page.live_count() × page.block_size()` per page. One
multiply, per **page**, inside a walk sweep already performs — nothing per
object, and nothing that touches a survivor. ADR-103's headline property is that
sweep does not touch survivors, and reconstructing the same number from the
objects would reintroduce precisely the O(live) walk ADR-103 deleted.

**It is block bytes only.** The `Box<str>` behind a `Text` and the `HashMap`
table behind a `Map` are charged against `bytes_since_collect` at allocation —
they must be, or a text-heavy program under-reports its pressure by essentially
its whole footprint — but they are not recoverable at sweep without an
`owned_bytes_of` call per survivor. The alternative, a running `live_owned`
counter decremented in sweep's dead-block loop, drifts: a `Vec` that grows after
allocation dies owning more than it was charged, so the counter saturates at
zero and stops meaning anything.

**The asymmetry is safe, and safe in the direction that matters.**
Under-counting `live` makes the next threshold *smaller*, so the collector runs
**more** often, never less. It cannot produce an unbounded heap. It can only
cost time, and only on a program whose live set is mostly owned bytes — where
mark cost is O(live *objects*), which is small by construction for exactly that
shape. `hashwork` is the suite's instance of it and it comes out 6% *faster*,
not slower.

Immortals are excluded, because they are on pages sweep does not walk. That is
RT-04's second reading: an object no collection can reclaim exerts no pressure,
so it must not buy the program a larger budget either. Without the exclusion
every Praxis program would start with ADR-100's ~40 KiB interned small-`Int`
table counted as live. `an_immortal_is_not_counted_in_the_live_set` pins it.

`live_bytes` is on `HeapStats` rather than behind a `#[cfg(test)]` accessor,
because the property it makes checkable — a long-running program's heap stops
growing — is end-to-end, and the test for it belongs where generated code runs.

## Decision 4: `PRAXIS_GC_PACER` stays, and `run.py` refuses to run with it set

Both pacers ship in one binary behind an environment variable read once per
process. **This is the first `std::env` read in any `src` file in this
workspace**, so it owes an argument rather than a convenience.

The argument for deleting it after the measurement is real, and it is what the
"make illegal states unrepresentable" maxim wants: a collector whose schedule
depends on the ambient environment is a reproducibility hazard, and this
codebase removes its instruments after use — `BlockLayout` withholds `Hash` so
that re-introducing a hash lookup is a compile error, and
`gcfix-pre-perf-fixes.json` was *renamed* specifically so a stale file could not
be silently compared against a current run.

It is kept anyway, for three reasons, with the hazard closed directly.

1. **The instrument is what made this item cheap, and the lack of one is what
   made it expensive.** The `fd70374` experiment was a scratch patch built into
   a separate target directory and reverted, so its result was unreproducible
   and unattributable — which is why handover 23 had to say "re-run it before
   anyone trusts it", and why re-running it was the bulk of this item. With both
   arms in one binary the A/B is a single build, and that is what bought a
   four-ceiling screening sweep and a `k` sweep instead of one number.
2. **A single compiled-in constant is already known to be wrong for at least one
   shape of program.** `pipeline` costs 20% at `k = 2` and 9% at `k = 4`, while
   the other seven benchmarks do not move; the mechanism (below) says why, and
   says the right setting is workload-dependent rather than discoverable.
   Shipping one number and no way to change it would be claiming otherwise.
3. **The ceiling is a measured constant on one machine**, and handover 23 §3
   lists two more one-constant questions of exactly this kind (`PAGE_SIZE`, and
   the benchmark sizes). Re-measuring it should not require a patch.

The hazard — that a stale export silently changes a recorded measurement — is
closed where it lives: **`run.py` refuses to run at all when `PRAXIS_GC_PACER`
is set.** `results.json` must measure the collector this workspace ships, and a
measurement that cannot say which build it came from must not be recorded. That
is the same guard `gcfix-pre-perf-fixes.json`'s deliberate filename is, applied
to the knob rather than to a file.

Two smaller things make it safe to have. An unparseable value prints one line to
stderr and falls back — a silent fallback would let a typo in one arm of an A/B
measure the other build and report the result as if it were the right one, and
`pacer_ab.py` greps for that line and fails the run. And `Pacer::bounded` is the
only constructor of the bounded arm; it clamps the ceiling up to
`INITIAL_COLLECT_THRESHOLD` and the factor up to 1, so "a ceiling below the
first threshold" and "zero headroom" have no spelling however hostile the
environment is.

## Measurements

Apple M2 Pro, 16 GiB, macOS 26.5.2, release build. **One binary**, both arms
selected by `PRAXIS_GC_PACER`, so no difference in optimization, layout or link
order can be read as a difference in pacing. The two arms run back to back
within each rep and **which one goes first alternates** on successive reps, so
the several-percent drift handover 23 §5 warns about is shared rather than
charged to whichever always ran second. Minimum per arm. Every run's stdout was
compared against `results.json`'s checksum; all matched. Peak RSS is one
`/usr/bin/time -l` run per (benchmark, arm), taken after the timed runs, through
`run.py`'s own `peak_rss_bytes`. `benchmarks/pacer_ab.py` is the harness and
`benchmarks/gcfix.json` holds both arms of the confirmation run.

**The prediction was written down before the numbers were read**, because the
whole item turns on whether the −14% still holds. It was: total sweep-and-relink
work is O(total allocated) and *independent* of the ceiling — collections scale
as `total/ceiling` while pages walked per collection scale as
`ceiling/PAGE_SIZE` — so the only ceiling-dependent cost is the per-collection
fixed cost, against which the bounded arm saves the first-touch zero-fill faults
on a multi-gigabyte peak. That predicted **under ~1% wall-clock cost and a 5–40×
RSS cut at a 64 MiB ceiling**, with the falsification condition stated: *if the
measured cost lands anywhere near −14%, the explanation is wrong and the result
must be re-diagnosed before anything ships.*

### Choosing the ceiling — `collatz`, `tree`, `hashwork`, full sizes, 3 reps, `k = 2`

Three shapes on purpose: `collatz` holds nothing (pure threshold effect), `tree`
holds 131,071 records (exercises `live × k` through blocks), `hashwork` holds
65,536 map entries (exercises the owned-bytes blind spot of decision 3).

| ceiling | `collatz` | `tree` | `hashwork` | time (geo) | peak RSS (geo) |
|---|---:|---:|---:|---:|---:|
| 8 MiB | 1.050× | 0.885× | 0.953× | 0.960× (−4.0%) | 42.2× less |
| 16 MiB | 1.036× | 0.931× | 0.940× | 0.968× (−3.2%) | 32.7× less |
| **64 MiB** | 1.005× | 0.921× | 1.042× | **0.988× (−1.2%)** | **14.4× less** |
| 256 MiB | 1.013× | (1.340×) | 1.055× | (1.127×) | 4.3× less |

Above 1.00× the bounded arm is faster. The trade is monotone and the knee is at
64 MiB: the time cost falls into the machine's own noise there while the memory
saving is still an order of magnitude, and 256 MiB gives back three quarters of
the memory win for nothing measurable.

The 256 MiB row's `tree` figure is **not trustworthy and is not relied on**. Its
control arm's minimum was 4.206 s where the same binary on the same workload
measured 3.27–3.43 s in every other pass of the session — a 28% excursion in the
arm that cannot be affected by the variable under test. It is recorded rather
than dropped, because a discarded outlier nobody can see is how a measurement
stops being one.

### The confirmation — all eight, full `sizes.json` sizes, 5 reps

`bfs` is included and is sound here: the handover's "too noisy to attribute" is
specifically about *reduced* size, and its three full-size samples in
`results.json` span 0.3%.

| benchmark | doubling | bounded 64 MiB | time | RSS doubling | RSS bounded | memory |
|---|---:|---:|---:|---:|---:|---:|
| `primes` | 1.512 s | 1.442 s | 1.049× | 966.7 MiB | 72.0 MiB | 13.4× less |
| `mandelbrot` | 2.941 s | 2.736 s | 1.075× | 2091.9 MiB | 72.5 MiB | 28.8× less |
| `collatz` | 1.277 s | 1.220 s | 1.047× | 507.0 MiB | 71.7 MiB | 7.1× less |
| `vm` | 7.292 s | 7.108 s | 1.026× | 1050.3 MiB | 73.2 MiB | 14.4× less |
| `hashwork` | 4.747 s | 4.475 s | 1.061× | 1604.8 MiB | 82.1 MiB | 19.5× less |
| `tree` | 3.427 s | 3.336 s | 1.027× | 2113.8 MiB | 96.8 MiB | 21.8× less |
| `pipeline` | 3.654 s | 4.595 s | 0.795× | 3142.0 MiB | 134.2 MiB | 23.4× less |
| `bfs` | 9.471 s | 9.452 s | 1.002× | 924.3 MiB | 105.3 MiB | 8.8× less |
| **geometric mean** | | | **1.006×** | | | **15.6× less** |

**The prediction survives and the −14% is gone.** Seven of eight benchmarks are
faster or unchanged; the suite is 0.6% *faster* on the geometric mean while
peaking 15.6× lower. The time win on the ones that hold nothing is the
page-fault saving the prediction named — `mandelbrot` gives back 7.5% for not
touching 2 GiB of fresh pages.

**The memory column is the deliverable, and its shape says more than its
magnitude.** The four benchmarks that hold nothing — `primes`, `mandelbrot`,
`collatz`, `vm` — all land within 1.5 MiB of each other at 72 MiB, which is the
64 MiB ceiling plus the process. The four that hold something come out ordered
by how much they hold: `hashwork` 82, `tree` 97, `bfs` 105, `pipeline` 134.
Resident set is now a function of what the program is holding plus a constant.
That is the sentence P-2 was opened to be able to write.

### The one benchmark that pays, and what the knob is for

`pipeline` costs 20.5%. It holds a 1,000,000-element source `Vec` live for its
whole run, so its `live × k` term is ~80 MiB and binds over the ceiling; its
bounded peak of 134 MiB is that threshold plus its live set, exactly as decision
2 predicts. Every one of its collections marks the whole million-element live
set, and it does so `1/k` times per byte allocated.

That mechanism names its own remedy, and the remedy was measured (`pipeline`,
`tree`, `hashwork`, 3 reps, ceiling 64 MiB):

| | `k = 2` | `k = 4` |
|---|---:|---:|
| `pipeline` | 0.795×, 134.2 MiB | 0.913×, 195.5 MiB |
| `tree` | 1.027×, 96.8 MiB | 0.986×, 96.3 MiB |
| `hashwork` | 1.061×, 82.1 MiB | 1.022×, 82.1 MiB |

Doubling `k` halves the mark cost per allocated byte and takes `pipeline`'s
penalty from 20% to 9%, while `tree` and `hashwork` do not move outside noise
and their peaks do not move at all — because for them `live × k` never wins.
That is the mechanism confirmed rather than assumed.

**`k = 2` is shipped anyway**, and that is a deliberate choice of the tighter
bound over the better average. `k` *is* the bound: peak is `(1 + k) × live`, so
`k = 4` converts a 3× guarantee into a 5× one for every program in the language
in order to buy back eleven points on one benchmark. Bounding the resident set
is the whole deliverable, and 2 is the same growth factor Go's default
`GOGC=100` picks for the same trade. The measurement is recorded here so that
the person with a mark-bound workload knows both the knob and its price.

## Consequences

- **Peak resident set is bounded by `live + max(64 MiB, live × 2)`.** What
  decides how large a Praxis program can be is no longer how much RAM the
  machine has. `benchmarks/REPORT.md`'s "what would move these numbers" item 2
  is closed, and its appendix now reports a landed change rather than a reverted
  experiment.
- **`benchmarks/results.json` measures the bounded collector**, and every Praxis
  peak-RSS figure in `REPORT.md` fell by an order of magnitude without a
  language semantic changing. The `sizes.json` workloads were chosen for a 6 GiB
  peak and are now far below what the machine can hold; they were deliberately
  **not** re-tuned here, because changing a size moves a benchmark to a
  different rung of the pacer's ladder, which is the thing being measured.
- **No ABI bump.** Nothing in generated code reads a `Heap` field offset —
  `lower.rs` takes offsets only from `RuntimeContext` and `EnumPayload` — so the
  two new fields cost nothing across the boundary. `RUNTIME_ABI_VERSION` is
  unchanged.
- **The page allocator's block-reuse path is now the common case, and it was
  exercised deliberately.** A bounded pacer turns "most allocations are fresh
  pages" into "most allocations are recycled blocks", and handover 23 §5 names
  the page allocator as the single largest untested surface in the tree — never
  run under a sanitizer, no nightly toolchain on this machine. The whole
  debug-profile suite (1,053 tests across `praxis-runtime`,
  `praxis-codegen-cranelift`, `praxis-debugger` and `praxis-cli`, with
  `block_index`'s Lemire-reciprocal `debug_assert` and `block_ptr`'s and
  `mark`'s live) was run once with `PRAXIS_GC_PACER=bounded:1M:2`, a ceiling 64×
  tighter than the shipped one. Green. That is not a substitute for ASan, and it
  is the strongest evidence this machine can produce.
- **`HeapStats` gained a field**, so anything constructing one by literal outside
  `heap.rs` would break. Nothing does.
- **No existing pacing test needed editing**, which was checked rather than
  assumed. `an_explicit_collection_does_not_grow_the_pacing_threshold` still
  reads `INITIAL × 2` after one paced collection, because it drives that
  collection with nothing rooted, so `live × k` is zero and the ratchet wins;
  `reset_restores_collection_pacing` is unaffected for the same reason. Both
  were re-verified under `PRAXIS_GC_PACER=bounded:1M:2` as well as under the new
  default. Everything else that touches pacing depends only on
  `INITIAL_COLLECT_THRESHOLD` or on "a collection happened", and a bounded pacer
  only ever collects *more* often, so it cannot make a "did it collect" test
  fail. If a future edit ever leaves objects rooted inside
  `allocate_until_paced`, both of those tests become order-dependent — that
  helper is the premise.
- **A mark-bound program is now the collector's worst case, where an
  allocation-bound one used to be.** `pipeline` is the suite's instance and it
  pays 20%. The next real improvement for that shape is not a pacer change; it
  is generational collection or escape analysis, and neither exists.
- **The live measurement is a lower bound, permanently.** Decision 3's
  under-count is safe, but it is real: a program whose footprint is mostly owned
  bytes gets a tighter schedule than its true live set warrants. If that ever
  needs fixing, the cost is an O(live) walk over survivors, and ADR-103 is the
  argument against it.
- **The doubling rule survives as a value, not as a possibility.**
  `Pacer::Doubling` has one use beyond the knob: the contrast arm of
  `a_bounded_heap_stops_growing`, which fails the bound the bounded arm meets.
  That is what makes the bound a measured property rather than an asserted one,
  and it is why the variant was not deleted along with the default.
