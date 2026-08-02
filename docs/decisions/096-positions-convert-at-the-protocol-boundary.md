# ADR-096: Positions convert at the protocol boundary; `LineMap` stays byte-based

**Date:** 2026-08-01
**Status:** accepted
**Milestone:** 11 (language server MVP)

## Context

An LSP `Position` is `(line, character)` where `character` counts **UTF-16 code
units** unless the client and server negotiate otherwise. `LineMap`'s `LineCol`
counts **bytes**, and says so in its own words: "a column is the byte offset
from the start of the line … this keeps the mapping lossless and O(1) to
invert."

There is no `utf16` anywhere in the workspace at `bcc5319`. So M11 introduces
the concept, and the only question is where it lives.

Pushing UTF-16 down into `praxis-source` would put a protocol concern under
every crate that reports a span — the lexer, the parser, inference, the input
parser, the renderer — to serve exactly one consumer. It would also make
`LineCol` ambiguous: a column would mean bytes in a diagnostic and code units in
a hover, in one type.

## Decision

**Negotiate `positionEncoding` at `initialize`, and convert in exactly one
`praxis-lsp` module (`praxis-lsp::position`), against the document text.**

- The server advertises `["utf-8", "utf-16"]` and selects UTF-8 when the client
  offers it, falling back to UTF-16 (the protocol default) otherwise. The
  selected encoding is stored once and read by the one conversion module.
- Conversion is `byte offset ⇄ Position`, both directions, parameterized by the
  encoding, computed from the document's own text.
- Nothing below `praxis-lsp` learns what a UTF-16 code unit is. `LineMap` is
  unchanged and still means bytes.

## Gate

A `proptest` property over arbitrary text — including astral-plane characters
(which are two UTF-16 code units and four UTF-8 bytes), CRLF, and a multi-byte
template interior — asserting:

1. `byte → position → byte` is the identity, in **both** encodings, for every
   character boundary in the text;
2. the two encodings agree exactly where the text is ASCII.

**The fixture corpus is deliberately not English-only.** This is the class of
bug that is invisible in every ASCII fixture: an implementation that returns the
byte column for `character` passes every test written in English and puts the
squiggle in the wrong place the first time a program contains `é`.

## Consequences

- A client that offers neither encoding gets UTF-16, the protocol default, and
  the server still works.
- `praxis-source::LineMap` gains no dependency and no parameter; the milestone
  adds no column ambiguity to the crate every diagnostic goes through.
- The conversion cost is per-request and proportional to the line, not the file:
  the module finds the line with `LineMap` (a binary search) and then counts
  within it. A file whose lines are ASCII costs a length comparison.
