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
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::Arc;

// Reader surface.
use elivagar::pmtiles_reader::{
    ArchiveView as LinkedArchiveView, PmtilesReader as LinkedPmtilesReader,
};
pub use elivagar::pmtiles_reader::{BlobRef, RawDirEntry, find_entry};
// Writer surface (mutate) + PMTiles addressing math.
pub use elivagar::pmtiles_writer::{PmtilesConfig, PmtilesWriter, tile_id_to_zxy, xy_to_tile_id};
// Decoder surface. DetailAttr/CanonRingRole/Strictness + decode_detail_attr feed
// the streaming canonical hash; the full wire-order DetailTile decoder feeds the
// detail canonical form (regress's "same tile", pinned against streaming by the
// equivalence tests).
pub use elivagar::tile_detail::{
    CanonRingRole, DetailAttr, DetailComponent, DetailFeature, DetailLayer, DetailRing, DetailTile,
    Strictness,
};
// MVT cursor and encoder primitives enter brokkr through this adapter as well,
// so corpus code never depends on protohoggr directly.
pub use protohoggr::{WIRE_32BIT, WIRE_64BIT, WIRE_LEN, WIRE_VARINT, encode_varint};
// The tile encoders serve the fixtures only.
#[cfg(test)]
pub use protohoggr::{encode_bytes_field_always, encode_tag, encode_varint_field_always};

/// Convert a panic in the linked archive reader into the same error channel as
/// malformed archive data. The linked PMTiles directory decoder is fallible
/// for ordinary corruption, but its parser also contains panic-capable paths.
fn linked_io<T>(operation: &'static str, f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("linked elivagar {operation} panicked"),
        )),
    }
}

/// Convert a panic in the linked tile decoder into its established `String`
/// error channel. Successful decode results pass through unchanged.
fn linked_decode<T>(
    operation: &'static str,
    f: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(_) => Err(format!("linked elivagar {operation} decoder panicked")),
    }
}

fn linked_wire<T>(
    operation: &'static str,
    f: impl FnOnce() -> protohoggr::WireResult<T>,
) -> protohoggr::WireResult<T> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(_) => Err(protohoggr::WireError { msg: operation }),
    }
}

/// Panic-contained cursor surface used to read archive tile bytes.
#[derive(Clone, Debug)]
pub struct Cursor<'a>(protohoggr::Cursor<'a>);

impl<'a> Cursor<'a> {
    #[must_use]
    pub fn new(data: &'a [u8]) -> Self {
        Self(protohoggr::Cursor::new(data))
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn read_varint(&mut self) -> protohoggr::WireResult<u64> {
        linked_wire("protobuf varint decoder panicked", || self.0.read_varint())
    }

    pub fn read_tag(&mut self) -> protohoggr::WireResult<Option<(u32, u32)>> {
        linked_wire("protobuf tag decoder panicked", || self.0.read_tag())
    }

    pub fn read_len_delimited(&mut self) -> protohoggr::WireResult<&'a [u8]> {
        linked_wire("protobuf field decoder panicked", || {
            self.0.read_len_delimited()
        })
    }
}

/// Panic-contained facade over elivagar's memory-mapped PMTiles reader.
pub struct ArchiveView(LinkedArchiveView);

impl ArchiveView {
    pub fn open(path: &Path) -> io::Result<Self> {
        linked_io("PMTiles archive reader", || LinkedArchiveView::open(path)).map(Self)
    }

    pub fn header(&self) -> &[u8; elivagar::pmtiles_reader::HEADER_SIZE] {
        self.0.header()
    }

    pub fn min_zoom(&self) -> u8 {
        self.0.min_zoom()
    }

    pub fn max_zoom(&self) -> u8 {
        self.0.max_zoom()
    }

    pub fn tile_type(&self) -> u8 {
        self.0.tile_type()
    }

    pub fn tile_compression(&self) -> u8 {
        self.0.tile_compression()
    }

    pub fn num_addressed(&self) -> u64 {
        self.0.num_addressed()
    }

    pub fn read_all_runs(&self) -> io::Result<Vec<RawDirEntry>> {
        linked_io("PMTiles directory decoder", || self.0.read_all_runs())
    }

    pub fn raw_blob(&self, blob: BlobRef) -> io::Result<&[u8]> {
        linked_io("PMTiles blob reader", || self.0.raw_blob(blob))
    }

    pub fn raw_blob_at(&self, offset: u64, length: u32) -> io::Result<&[u8]> {
        linked_io("PMTiles blob reader", || self.0.raw_blob_at(offset, length))
    }

    pub fn metadata(&self) -> io::Result<String> {
        linked_io("PMTiles metadata decoder", || self.0.metadata())
    }
}

/// Panic-contained facade over elivagar's seek-based PMTiles reader.
pub struct PmtilesReader(LinkedPmtilesReader);

impl PmtilesReader {
    pub fn open(path: &Path) -> io::Result<Self> {
        linked_io("PMTiles reader", || LinkedPmtilesReader::open(path)).map(Self)
    }

    pub fn file_size(&self) -> io::Result<u64> {
        linked_io("PMTiles file metadata reader", || self.0.file_size())
    }

    pub fn internal_compression(&self) -> u8 {
        self.0.internal_compression()
    }

    pub fn num_addressed(&self) -> u64 {
        self.0.num_addressed()
    }

    pub fn num_entries(&self) -> u64 {
        self.0.num_entries()
    }

    pub fn num_unique(&self) -> u64 {
        self.0.num_unique()
    }

    pub fn min_zoom(&self) -> u8 {
        self.0.min_zoom()
    }

    pub fn max_zoom(&self) -> u8 {
        self.0.max_zoom()
    }

    pub fn root_dir_offset(&self) -> u64 {
        self.0.root_dir_offset()
    }

    pub fn root_dir_length(&self) -> u64 {
        self.0.root_dir_length()
    }

    pub fn leaf_dirs_offset(&self) -> u64 {
        self.0.leaf_dirs_offset()
    }

    pub fn leaf_dirs_length(&self) -> u64 {
        self.0.leaf_dirs_length()
    }

    pub fn data_length(&self) -> u64 {
        self.0.data_length()
    }

    pub fn read_all_runs(&mut self) -> io::Result<Vec<RawDirEntry>> {
        linked_io("PMTiles directory decoder", || self.0.read_all_runs())
    }

    pub fn read_directory(&mut self, offset: u64, length: u64) -> io::Result<Vec<RawDirEntry>> {
        linked_io("PMTiles directory decoder", || {
            self.0.read_directory(offset, length)
        })
    }
}

/// Decompress one gzip-compressed tile or PMTiles section without allowing a
/// panic in the linked decoder to escape the seam.
pub fn gzip_decompress(data: &[u8]) -> io::Result<Vec<u8>> {
    linked_io("gzip decoder", || {
        elivagar::pmtiles_reader::gzip_decompress(data)
    })
}

/// Read a little-endian i32 without exposing the linked reader's unchecked
/// slice indexing to callers.
pub fn read_i32_le(data: &[u8], offset: usize) -> io::Result<i32> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "i32 offset overflows"))?;
    let bytes = data
        .get(offset..end)
        .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::UnexpectedEof, "truncated little-endian i32")
        })?;
    Ok(i32::from_le_bytes(bytes))
}

/// Decode one MVT tile through the linked structural decoder.
pub fn decode_detail_tile(data: &[u8], strictness: Strictness) -> Result<DetailTile, String> {
    linked_decode("MVT tile", || {
        elivagar::tile_detail::decode_detail_tile(data, strictness)
    })
}

/// Decode one MVT feature through the linked structural decoder.
pub fn decode_detail_feature(
    data: &[u8],
    keys: &[Arc<str>],
    values: &[DetailAttr],
    strictness: Strictness,
) -> Result<DetailFeature, String> {
    linked_decode("MVT feature", || {
        elivagar::tile_detail::decode_detail_feature(data, keys, values, strictness)
    })
}

/// Decode one MVT value through the linked structural decoder.
pub fn decode_detail_attr(data: &[u8], strictness: Strictness) -> Result<DetailAttr, String> {
    linked_decode("MVT value", || {
        elivagar::tile_detail::decode_detail_attr(data, strictness)
    })
}

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
