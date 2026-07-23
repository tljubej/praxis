# ADR-022: Source-slice `Text` representation in M6

**Date:** 2026-07-23 · **Status:** accepted

## Context

§4.3 allows two `Text` representations: an owned UTF-8 payload (`Box<str>`,
shipped in M3 via ADR-013) and a *source-slice metadata* carrying
`(owner: GcRef, start, length)` per §7.10. ADR-013 deferred the slice layout to
M6 "when the input-parser produces source slices," recording that as a
deliberate deviation rather than a silent gap.

M6 delivers that input parser (§7). It evaluates a compiled `ParserPlan`
against the process-input buffer and allocates GC results — including
source-slice `Text`s for the `word`/`text`/`rest` atomics. These are
zero-copy views into the stdin buffer: the parser records byte offsets rather
than copying bytes, so a multi-megabyte input costs only a handful of `GcRef`s
plus two `usize`s per slice.

## Decision

- **Payload is a tagged enum.** `TextPayload` becomes
  `enum { Owned(Box<str>), Slice { owner: GcRef, start: usize, len: usize } }`
  (`praxis-runtime/src/text.rs`), `#[repr(C)]` so generated code can read it at
  a fixed layout.

- **One descriptor serves both variants.** The `TEXT` descriptor
  (`praxis-runtime/src/text.rs`) is unchanged in identity; its callbacks
  (`trace`, `format`, `equals`, `hash`) branch on the discriminant and go
  through two helpers — `text_bytes` / `text_str` — which *recurse* through
  slice owners until they reach an owned payload. There is no separate
  `TextSlice` descriptor.

  - `trace` traces the `owner` `GcRef` for `Slice` (keeping the backing alive)
    and is a no-op for `Owned`.
  - `format`, `equals`, and `hash` all reduce to `text_bytes`/`text_str`, so a
    slice compares equal to the owned `Text` spelling the same bytes.
  - `drop_value` runs `drop_in_place`: `Owned` releases its `Box<str>`;
    `Slice` drops only the `GcRef` (a no-op pointer copy) — the owner is
    GC-managed, not slice-owned.

- **`RuntimeContext` gains a `unit_ref` field** — the cached immortal `Unit`
  (`praxis-runtime/src/context.rs`). Previously `input_source` was overloaded
  as the fault sentinel (`unit_sentinel`): in M4, before any real input was
  installed, pointing it at the immortal `Unit` was harmless. Once `input_source`
  holds the real read-in buffer (M6), it can no longer double as `Unit`, so the
  sentinel reads the dedicated `unit_ref` instead. `unit_ref` is set at context
  creation (`Runtime::context`) and is stable for the program lifetime.

## Reason

- The headline soundness argument is the **non-moving collector** (ADR-011):
  object addresses never change, so following an `owner` pointer is safe for as
  long as the owner is reachable. The slice's `trace` callback keeps the owner
  reachable, and the input buffer itself is GC-rooted via
  `RuntimeContext.input_source` (installed once in `praxis-cli/src/run.rs`) and
  never mutated after installation.
- A single `TEXT` descriptor — rather than one per variant (the "split
  per-layout" alternative ADR-013 left open) — avoids a type switch at every
  `GcRef`'s descriptor lookup. The only cost is the discriminant branch inside
  each callback, which is negligible.
- `text_bytes`/`text_str` return `&'static [u8]` / `&'static str`: sound because
  the backing lives in the GC heap (non-moving) or the immortal input buffer,
  both of which outlive any borrow of the slice. The parser only splits on UTF-8
  scalar boundaries, so the slices are always valid UTF-8 by construction.
- Splitting `unit_ref` out of `input_source` keeps the fault sentinel stable
  regardless of whether input is present, decoupling two unrelated roles that
  M4 had conflated only because no input existed yet.

## Consequences

- **Zero-copy parsing:** `word`/`text`/`rest` reference the input buffer
  without copying bytes, so parsing large inputs allocates only metadata.
- **Source-aware faults** can report byte offsets into the input buffer, since
  every slice carries `(start, len)` relative to a known `owner`.
- The recursion in `text_bytes` is bounded by the slice-chain length
  (slices-of-slices); in practice the chain is shallow (the parser slices the
  owned input buffer directly). A follow-up could *flatten* slice chains by
  rewriting a `Slice { owner, .. }` whose owner is itself a slice into a slice
  pointing at the ultimate owned backing — this is an optimization, not a
  correctness need, and is deferred per rule 20.11.
- Slices-of-slices that survive across a collection still work: each hop's
  `trace` forwards to its owner, so a chain of any depth is correctly kept
  alive.
- A `Slice` whose `owner` is collected would be unsound; the `trace` callback
  is the only thing preventing that, so any future change to `TEXT.trace` must
  preserve the owner trace.
