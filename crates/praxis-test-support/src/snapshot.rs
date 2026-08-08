//! The canary for the file-snapshot half of the `insta` harness.
//!
//! Every other snapshot assertion in the workspace is inline (`@"…"`), so this
//! is the only test that exercises the `src/snapshots/*.snap` + bless workflow
//! the crate doc describes. It carries no production code of its own.

#[cfg(test)]
mod tests {
    #[test]
    fn snapshot_smoke_test() {
        // The harness itself must produce deterministic output.
        insta::assert_snapshot!("hello-snapshot", "stable output\n");
    }
}
