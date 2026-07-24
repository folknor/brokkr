//! The linked-elivagar-crate surface the corpus gate consumes.
//!
//! Per `elivagar.md` / `brokkr.md`, brokkr owns the gate but does not decode
//! PMTiles itself: it links the elivagar crate and reads archives in-process
//! through elivagar's reader/decoder/writer. This module is the seam - the one
//! place that names the elivagar API the gate depends on, so the rest of
//! `corpus/` imports from here rather than reaching into `elivagar::` directly.
//!
//! The canonical tile hash is NOT here: it is brokkr's (`corpus/canonical.rs`),
//! built over the `decode_detail_attr` + `protohoggr` primitives this seam
//! re-exports. Likewise `next_zoom_boundary` is brokkr's - it was regress-
//! internal in elivagar and left with the adjudication layer.

// Reader surface.
pub use elivagar::pmtiles_reader::{
    ArchiveView, BlobRef, RawDirEntry, find_entry, gzip_decompress, read_i32_le,
};
// Writer surface (mutate) + PMTiles addressing math.
pub use elivagar::pmtiles_writer::{PmtilesConfig, PmtilesWriter, tile_id_to_zxy, xy_to_tile_id};
// Decoder surface. DetailAttr/CanonRingRole/Strictness + decode_detail_attr feed
// the streaming canonical hash; the full wire-order DetailTile decoder feeds the
// detail canonical form (regress's "same tile", pinned against streaming by the
// equivalence tests).
pub use elivagar::tile_detail::{
    CanonRingRole, DetailAttr, DetailComponent, DetailFeature, DetailLayer, DetailRing, DetailTile,
    Strictness, decode_detail_attr, decode_detail_feature,
};

/// The tile id that begins the zoom after the one holding `tile_id`. Brokkr-
/// owned (was `regress`-internal in elivagar; left with the adjudication layer).
#[must_use]
pub fn next_zoom_boundary(tile_id: u64) -> u64 {
    let (z, _, _) = tile_id_to_zxy(tile_id);
    if z >= 30 {
        return u64::MAX;
    }
    let n = 1u64 << z;
    (n * n * 4 - 1) / 3
}
