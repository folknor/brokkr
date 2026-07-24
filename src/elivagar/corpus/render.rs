//! The canonical SVG render core, the classifyRings port, and the style
//! machinery - brokkr's per the redesign (`elivagar.md`, "Code that leaves the
//! crate"). It answers to cross-host byte identity and MapLibre semantics (the
//! 500-ring clamp) and never grows viewer conveniences; the viewer-flavoured
//! `elivagar svg` is a separate renderer that must not converge with this one.
//!
//! **Cross-host byte-identity obligation** (`brokkr.md`, `tiles/*.svg`): integer
//! -only path emission, deterministic iteration order everywhere, fixed colour
//! and number formatting. `check`'s SVG-staleness step byte-compares fresh
//! renders against the committed files on whatever host runs it, so a
//! formatting-nondeterministic renderer is a design rejection.
//!
//! SCAFFOLD: the render core and style parser decode tiles and therefore wait on
//! the elivagar decoder API (`eliv.rs`). The type shapes here are the intended
//! surface; bodies are `unimplemented!()` until that lands and the classifyRings
//! + style port is lifted in.

use std::io;
use std::path::Path;

use super::super::eliv::ArchiveView;

/// The canonical corpus render style, loaded from `corpus/style.toml`. Its
/// `hash_hex` is the recorded style identity brokkr authors into `contract.json`
/// and re-checks in the staleness step.
pub struct Style {
    _private: (),
}

#[allow(clippy::unused_self, unused_variables, clippy::missing_errors_doc)]
impl Style {
    /// Load and validate the canonical style.
    pub fn load(path: &Path) -> io::Result<Self> {
        unimplemented!("render core pending elivagar decoder API: Style::load (render.rs)")
    }
    /// xxh3-128 of the canonical style, lowercase hex - the recorded identity.
    #[must_use]
    pub fn hash_hex(&self) -> String {
        unimplemented!("render core pending: Style::hash_hex")
    }
}

/// One rendered tile: the SVG bytes plus any non-fatal render warnings
/// (unstyled layers, ring-clamp hits) surfaced up to the caller.
pub struct RenderedTile {
    pub bytes: Vec<u8>,
    pub warnings: Vec<String>,
}

/// Render a single tile of an open archive to canonical SVG. `layers = None`
/// renders every layer; `Some(list)` restricts to the named layers.
#[allow(unused_variables, clippy::missing_errors_doc)]
pub fn render_archive_tile(
    view: &ArchiveView,
    z: u8,
    x: u32,
    y: u32,
    style: &Style,
    layers: Option<&[String]>,
) -> io::Result<RenderedTile> {
    unimplemented!("render core pending elivagar decoder API: render_archive_tile (render.rs)")
}
