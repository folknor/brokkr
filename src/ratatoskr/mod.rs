//! Ratatoskr-specific brokkr commands.
//!
//! The eventual harness lives partly here and partly in ratatoskr's own
//! source tree (the side that holds `ServiceClient`). This module owns
//! brokkr's orchestration responsibilities: project gating, build
//! coordination via `[[check]]` sweeps, lockfile, artefact-dir
//! lifecycle, history recording. The Lua VM and the `ServiceClient`
//! Lua bindings live in ratatoskr (in the `app` crate, behind the
//! `test-helpers` feature). The cross-cutting design lives at
//! `<ratatoskr>/docs/harness/{problem-statement,architecture,roadmap}.md`.

pub mod build;
pub mod cmd;
pub mod discover;
pub mod gate;
pub mod process;
pub mod saehrimnir;
pub mod sync;
