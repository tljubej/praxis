# Architectural decisions

Per the Praxis technical design (§20, rule 1), the design document is the
current contract and deliberate deviations are recorded here before
implementing them. Each entry is short: context, decision, and the reason.

Entries are numbered and dated. Add new decisions at the bottom.

- [ADR-001: Snapshot testing library is `insta`](./001-snapshot-testing-insta.md)
- [ADR-002: CI runs `just ci`, with a minimal GitHub Actions wrapper](./002-ci-via-just.md)
- [ADR-003: Lossless syntax tree uses the `rowan` crate](./003-lossless-tree-uses-rowan.md)
- [ADR-004: Hand-written recursive-descent parser with Pratt climbing](./004-parser-technique.md)
- [ADR-005: Formatter skeleton lives in `praxis-parser`](./005-formatter-lives-in-praxis-parser.md)
- [ADR-006: Milestone 1 fuzz gate is a `proptest` property test](./006-m1-fuzz-via-proptest.md)
