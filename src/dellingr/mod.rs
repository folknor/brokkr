//! Dellingr benchmarking: `brokkr dellingr --lua <workload>`.
//!
//! Dellingr is a single-crate pure-Rust Lua VM, and its bench surface is the
//! narrowest of any project here. There are no external datasets, no host data
//! dirs and no scratch tree: a workload is a short `.lua` file tracked in the
//! repo, and a run is single-threaded, CPU-bound and does no I/O. Most of what
//! the pbfhogg/elivagar paths exist to resolve simply doesn't apply.
//!
//! What *is* specific to dellingr:
//!
//! - **The harness is a cargo example, not a bin.** `[dellingr] example` names
//!   the target; the mode picks its features (`--bench` bare, `--hotpath` and
//!   `--alloc` adding the standard hotpath features).
//! - **Workloads are hash-pinned.** They are editable source rather than
//!   immutable input data, so an edit would silently redefine every stored row
//!   filed under the same name. [`workload::resolve`] verifies the digest and
//!   refuses on drift.
//! - **Instrumented modes resolve a different file.** `--hotpath` / `--alloc`
//!   require and resolve the workload's `hotpath_file` / `hotpath_xxh128`
//!   pair - an instrumentation-scale variant of the same kernel - because the
//!   hotpath crate's per-call event queue is unbounded and a seconds-scale
//!   `file` backlogs tens of GB of RAM under instrumentation. See
//!   [`crate::config::DellingrWorkload`].
//! - **A `--commit` baseline mixes trees on purpose.** The harness is built
//!   from the old worktree; the workload still comes from the registration in
//!   the current tree. See [`workload::resolve`].

pub(crate) mod cmd;
pub(crate) mod workload;
