//! The canary for the file-snapshot half of the `insta` harness.
//!
//! Every other snapshot assertion in the workspace is inline (`@"…"`), so this
//! is the only test that exercises the `src/snapshots/*.snap` + bless workflow
//! the crate doc and handover 00 describe.
//!
//! It has no production code to keep company. Two diagnostic-rendering helpers
//! used to live here: `render_diagnostics`, a join-with-blank-lines loop over
//! [`praxis_source::Renderer`], and `snapshot_diagnostics`, a one-line alias
//! for it. Neither ever acquired a caller outside this file. The loop that is
//! actually run is praxis-cli's `render_all`, which differs in all three of
//! renderer (`new_styled` with a palette), separator guard and severity tally,
//! so there was nothing to fold into it either.

#[cfg(test)]
mod tests {
    #[test]
    fn snapshot_smoke_test() {
        // The harness itself must produce deterministic output.
        insta::assert_snapshot!("hello-snapshot", "stable output\n");
    }
}
