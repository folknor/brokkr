//! `project = "piners"` support: the `brokkr corpus` parity-corpus runner.
//!
//! See `docs/commands/corpus.md`.
//!
//! `corpus` is not a measurable command: it takes no `--bench`, `--hotpath`
//! or `--alloc`. A measured path existed and was retired because piners
//! retired its side. piners removed the corpus binary's hotpath guard and
//! counting allocator (hotpath's 64-frame ceiling aborts on deep Pine
//! recursion) and keeps hotpath out of the corpus build; `piners-harness`
//! defines no `hotpath-alloc` feature, and its corpus binary forks a process
//! per probe, so one report per iteration could not profile it anyway. The
//! run row's `wall_ms` in `runs.db` is the corpus timing. Profiling the
//! corpus again needs a producer and capture contract agreed with piners
//! first.

pub mod bless;
pub mod cmd;
pub mod contract;
pub mod corpus_db;
pub mod corpus_query;
pub mod gate;
pub mod integrity;
pub mod isolate;
pub mod lfs;
pub(crate) mod line_value;
pub mod manifest;
pub mod performance;
pub mod pins_write;
pub(crate) mod prepare;
pub mod registry;
pub mod registry_io;
pub mod report;
pub mod reseed;
pub mod run_model;
pub mod schedule;
pub mod select;
pub(crate) mod selector;
pub(crate) mod table;
pub mod time;
pub(crate) mod toml_writer;

pub(crate) mod db_paths;
