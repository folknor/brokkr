//! `brokkr mogwai` - benchmark a mogwai surface.
//!
//! Two kinds of surface, one row shape, no layers.
//!
//! ARGV-SHAPED surfaces go through the shipped bin: `gen`, `tick-composition`,
//! `preflight`, `measure`, `fit`, `cache`, `synth`, `arrival-screen`. A
//! process, an argv, a wall. Benching them through the release binary measures
//! what ships - startup and argument parsing included - which is the honest
//! end-to-end number. None of them are registered: the bin is registered once
//! and the argv is composed at the call site.
//!
//! HARNESS-SHAPED surfaces go through a cargo example: the matching loop and
//! divergence seam, the `TickSource` implementations, the arrival draw, the
//! screen's projection, and eventually the serving path. These have no command
//! line, so there is nothing an argv registry could hold - the harness itself
//! is the addressable thing, and `[mogwai.targets.*]` names it.
//!
//! The predecessor registered a name per hand-written argv, which addressed
//! only the first kind and forced the second - the majority of the eventual
//! surface - into a "layer 2" that was an escape hatch with a name. What a
//! registry must hold is only what the invocation cannot recover: which cargo
//! target a name means, and which features it needs to be built with. That
//! second half is why `--hotpath` and `--alloc` previously recorded rows with
//! no profile in them.
//!
//! Rows carry the invocation verbatim in `cli_args` and `brokkr_args`, so
//! pairing is a query - `--grep` selects an arm, including the arm defined by
//! an *absent* flag - rather than a name lookup that can lie.

pub(crate) mod cmd;
pub(crate) mod targets;
