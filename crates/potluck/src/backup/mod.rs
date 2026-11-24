//! Online backup: a consistent copy of a running server's data directory, streamed as a tar.
//!
//! Each workspace repository is copied by a local `git fetch` into a staging directory, the
//! database is snapshotted with SQLite's backup API, and the staged tree is written as a tar.

#[expect(dead_code, reason = "wired by write_backup")]
mod copy;
#[expect(dead_code, reason = "wired by write_backup")]
mod snapshot;
