//! Shared by the integration tests: where the C reference oracle is, and how to run it.
//!
//! The differential tests are the reason this port can claim byte-identity, so the policy for
//! finding the oracle lives here once: unset means "skip, the suite stays dependency-free", but
//! a path that does not exist is a mistake in the caller's setup and must not pass silently —
//! a typo that turned every differential test into a no-op would be the worst outcome of all.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The oracle binary, or `None` when `LZ4_C_ORACLE` is unset (the caller then skips).
///
/// Prints one line saying what is being skipped, so a run that never touched the C reference
/// says so instead of looking like a pass.
pub fn oracle_or_skip(what: &str) -> Option<PathBuf> {
    let Some(value) = std::env::var_os("LZ4_C_ORACLE") else {
        eprintln!("LZ4_C_ORACLE unset: skipping {what}");
        return None;
    };
    let path = PathBuf::from(value);
    if !path.exists() {
        panic!(
            "LZ4_C_ORACLE points at {} which does not exist (build it with tools/build_oracle.sh)",
            path.display()
        );
    }
    Some(path)
}

/// Run the oracle; returns whether it reported success. A failure to *start* it is a panic:
/// the path came from `oracle_or_skip`, so it exists but is not runnable — a setup problem.
pub fn oracle_run<I, S>(oracle: &Path, args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    std::process::Command::new(oracle)
        .args(args)
        .status()
        .unwrap_or_else(|e| panic!("{}: {e}", oracle.display()))
        .success()
}
