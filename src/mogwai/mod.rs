//! `brokkr mogwai` - the layer-1 regression bench.
//!
//! The durable numbers: one row series per named workload, tracked across
//! months, paired with the commit that produced them.
//!
//! The unit here is a frozen INVOCATION rather than a subcommand mirror or a
//! pinned file. Mogwai's commands take config-shaped arguments (preset, window,
//! seed set, probe mode), so no single file's digest stands for "the work" the
//! way dellingr's `.lua` does, and mirroring the subcommand set would file rows
//! under a name that says nothing about which arguments produced them. The argv
//! stated in full is the contract instead.
//!
//! Two properties of that contract are load-bearing and easy to lose:
//!
//! - Rows are filed under the WORKLOAD NAME, with the invocation kept out of
//!   `brokkr_args`. `pair_key` includes `brokkr_args` on purpose - flags like
//!   `--direct-io` define genuinely different arms elsewhere - so letting the
//!   expanded argv reach it would mean any edit to `brokkr.toml` silently
//!   stopped a workload's new rows from pairing with its old ones. Silently,
//!   because unpaired rows render as two one-sided lines rather than an error.
//! - The dataset column stays empty for generated workloads, following
//!   dellingr: the workload is the operation, not a file.

pub(crate) mod cmd;
pub(crate) mod workload;
