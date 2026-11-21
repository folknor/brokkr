//! Typed Cargo and rustc facts shared by brokkr commands.
//!
//! [`metadata`] memoizes one `cargo metadata` snapshot per root, query and
//! environment for the invocation, with the indexed dependency graph;
//! `metadata::load_fresh` bypasses the cache for a script bench's per-arm
//! target resolution. [`rustc`] parses `rustc -vV` and caches it per root,
//! run from the project's directory so rustup selects its toolchain.

pub(crate) mod metadata;
pub(crate) mod rustc;
