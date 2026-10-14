//! The linked-elivagar-crate surface used by brokkr's archive consumers.
//!
//! Per `elivagar.md` / `brokkr.md`, brokkr owns the gate but does not decode
//! PMTiles itself: it links the elivagar crate and reads archives in-process
//! through elivagar's reader/decoder/writer. This module is the seam - the one
//! place that names the elivagar API brokkr depends on, so `corpus/`,
//! `regress/` and the PMTiles adapters import from here rather than reaching
//! into `elivagar::` directly. brokkr's durable archive paths and lifecycle
//! are not part of this seam; they live in `elivagar/archive_store.rs`.
//!
//! The canonical tile hash is NOT here: it is brokkr's (`corpus/canonical.rs`),
//! built over the `decode_detail_attr` + `protohoggr` primitives this seam
//! re-exports. Likewise `next_zoom_boundary` is brokkr's - it was regress-
//! internal in elivagar and left with the adjudication layer.

use std::io;

// Reader surface.
pub use elivagar::pmtiles_reader::{
    ArchiveView, BlobRef, PmtilesReader, RawDirEntry, find_entry, gzip_decompress, read_i32_le,
};
// Writer surface (mutate) + PMTiles addressing math.
pub use elivagar::pmtiles_writer::{PmtilesConfig, PmtilesWriter, tile_id_to_zxy, xy_to_tile_id};
// Decoder surface. DetailAttr/CanonRingRole/Strictness + decode_detail_attr feed
// the streaming canonical hash; the full wire-order DetailTile decoder feeds the
// detail canonical form (regress's "same tile", pinned against streaming by the
// equivalence tests).
pub use elivagar::tile_detail::{
    CanonRingRole, DetailAttr, DetailComponent, DetailFeature, DetailLayer, DetailRing, DetailTile,
    Strictness, decode_detail_attr, decode_detail_feature, decode_detail_tile,
};
// MVT cursor and encoder primitives enter brokkr through this adapter as well,
// so corpus code never depends on protohoggr directly.
pub use protohoggr::{Cursor, WIRE_32BIT, WIRE_64BIT, WIRE_LEN, WIRE_VARINT, encode_varint};
// The tile encoders serve the fixtures only.
#[cfg(test)]
pub use protohoggr::{encode_bytes_field_always, encode_tag, encode_varint_field_always};

/// Read one protobuf varint while preserving the caller's byte cursor. The
/// mutation tool uses this for field surgery, where unknown fields must retain
/// their original payload bytes and cannot be normalized through tile decode.
pub(crate) fn read_varint(input: &[u8], at: &mut usize) -> io::Result<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *input.get(*at).ok_or_else(|| {
            io::Error::new(io::ErrorKind::UnexpectedEof, "truncated protobuf varint")
        })?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "protobuf varint overflows u64",
    ))
}

/// The tile id that begins the zoom after the one holding `tile_id`. Brokkr-
/// owned (was `regress`-internal in elivagar; left with the adjudication layer).
#[must_use]
pub fn next_zoom_boundary(tile_id: u64) -> u64 {
    let (z, _, _) = tile_id_to_zxy(tile_id);
    if z >= 32 {
        return u64::MAX;
    }
    let n = 1u64 << z;
    let square = n * n;
    // (4 * square - 1) / 3, arranged so z31 does not overflow before the
    // division even though its next-zoom boundary still fits in u64.
    ((square - 1) / 3) * 4 + 1
}
