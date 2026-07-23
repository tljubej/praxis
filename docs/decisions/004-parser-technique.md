# ADR-004: Hand-written recursive-descent parser with Pratt climbing

**Date:** 2026-07-23 · **Status:** accepted

## Context

Milestone 1 (§19) needs a parser for literals, bindings, blocks, calls,
functions, arithmetic, `if`, `while`, and `out` calls, with error recovery
"sufficient for LSP use." The parser technique must support precise diagnostics,
recovery that yields multiple diagnostics from one malformed file, and the
precedence/associativity rules of §4.12.

## Decision

Write the parser by hand: recursive descent for statements and structural
constructs, with a Pratt (precedence-climbing) loop for arithmetic and other
binary operators. The parser emits events into a `rowan::GreenNodeBuilder`
(see ADR-003) and produces `Diagnostic`s with category `Parse` (`P0xx`).

## Reason

- Hand-written recursive descent gives full control over diagnostic spans and
  messages — essential for the quality bar (§8.2) and for LSP (§15.2).
- Pratt climbing is the simplest correct way to encode operator precedence and
  associativity without a table-driven generator or left-recursion gymnastics.
- Error recovery is local and explicit: on an unexpected token, emit a `P0xx`
  diagnostic, wrap the stray token in an error node, advance to a
  synchronization point, and continue. This directly satisfies "parser produces
  multiple diagnostics from one malformed file" and "never panic."
- No build step, grammar file, or code generator is introduced — the design
  (§20, rule 10) prefers simple closed implementations over extensible
  mechanisms.

## Consequences

- The grammar lives in `praxis-parser` as Rust code; adding a construct is a
  code change plus a golden-tree snapshot, not a grammar edit.
- Precedence and associativity are encoded in a table consulted by the Pratt
  loop; getting them wrong shows up as a snapshot diff, so they are easy to
  verify and hard to silently regress.
