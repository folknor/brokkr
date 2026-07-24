//! The linked-elivagar-crate surface the corpus gate consumes.
//!
//! Per `elivagar.md` / `brokkr.md`, brokkr owns the gate but does not decode
//! PMTiles itself: it links the elivagar crate and reads archives in-process
//! through elivagar's reader/decoder/writer. This module is the seam - the one
//! place that names the elivagar API the gate depends on.
//!
//! **Scaffold state (elivagar API pending).** Until elivagar exposes the public
//! library surface (`elivagar.md`, "The library surface elivagar must expose"),
//! the archive/decoder/writer entry points here are `unimplemented!()` stubs so
//! the gate compiles and its logic can be built out. The pure addressing math
//! (Hilbert tile-id <-> z/x/y, zoom boundaries) is real: it is not decode, and
//! the fold and formats need it now. When the crate lands, the stub bodies below
//! become thin `elivagar::...` calls; nothing in `corpus/` should change.
//!
//! One deliberate scaffold compromise: `semantic_hash` is named here as part of
//! the decoder surface. Contractually the *canonicalization rules* are brokkr's
//! (they define "same tile"); they will be lifted out of this shim into
//! `corpus/canonical.rs` over elivagar's exposed tile decoder once it exists.

// This module is a forward declaration of the elivagar library surface: several
// items (HEADER_SIZE, PmtilesConfig fields, some reader methods) are consumed
// only once the crate is linked and the stub bodies become real calls.
#![allow(dead_code)]

use std::io;
use std::path::Path;

/// PMTiles v3 header length. Mirrors `elivagar::pmtiles_reader::HEADER_SIZE`.
pub const HEADER_SIZE: usize = 127;

/// One directory run: a contiguous span of tile ids sharing one stored blob.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RawDirEntry {
    pub tile_id: u64,
    pub offset: u64,
    pub length: u32,
    pub run_length: u32,
}

/// A reference to one stored tile payload. Multiple runs may point at one blob.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct BlobRef {
    pub offset: u64,
    pub length: u32,
}

/// Little-endian i32 read out of the header. Mirrors the elivagar reader helper.
#[must_use]
pub fn read_i32_le(buf: &[u8], off: usize) -> i32 {
    i32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

// ---------------------------------------------------------------------------
// Pure addressing math - real, not decode. Copied verbatim from elivagar's
// pmtiles_writer so the fold produces byte-identical baselines; when the crate
// is linked these become re-exports.
// ---------------------------------------------------------------------------

/// Convert (z, x, y) to a PMTiles Hilbert tile id.
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub fn xy_to_tile_id(z: u8, x: u32, y: u32) -> u64 {
    if z == 0 {
        return 0;
    }
    let n = 1u64 << z;
    let base = (n * n - 1) / 3;
    base + hilbert_xy2d(n as u32, x, y)
}

/// Convert a PMTiles Hilbert tile id back to (z, x, y).
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub fn tile_id_to_zxy(tile_id: u64) -> (u8, u32, u32) {
    if tile_id == 0 {
        return (0, 0, 0);
    }
    let mut z: u8 = 0;
    loop {
        z += 1;
        if z >= 31 {
            break;
        }
        let n = 1u64 << z;
        let next_base = (n * n * 4 - 1) / 3;
        if tile_id < next_base {
            break;
        }
    }
    let n = 1u64 << z;
    let base = (n * n - 1) / 3;
    let (x, y) = hilbert_d2xy(n as u32, tile_id - base);
    (z, x, y)
}

/// The tile id that begins the zoom after the one holding `tile_id`.
#[must_use]
pub fn next_zoom_boundary(tile_id: u64) -> u64 {
    let (z, _, _) = tile_id_to_zxy(tile_id);
    if z >= 30 {
        return u64::MAX;
    }
    let n = 1u64 << z;
    (n * n * 4 - 1) / 3
}

#[allow(clippy::cast_possible_truncation)]
fn hilbert_xy2d(n: u32, x: u32, y: u32) -> u64 {
    let mut d: u64 = 0;
    let (mut x, mut y) = (x, y);
    let mut s = n / 2;
    while s > 0 {
        let rx: u32 = u32::from((x & s) > 0);
        let ry: u32 = u32::from((y & s) > 0);
        d += (u64::from(s) * u64::from(s)) * u64::from((3 * rx) ^ ry);
        hilbert_rot(n, &mut x, &mut y, rx, ry);
        s /= 2;
    }
    d
}

fn hilbert_d2xy(n: u32, d: u64) -> (u32, u32) {
    let mut x: u32 = 0;
    let mut y: u32 = 0;
    let mut d = d;
    let mut s: u32 = 1;
    while s < n {
        #[allow(clippy::cast_possible_truncation)]
        let val = (d & 3) as u32;
        let rx = u32::from(val >= 2);
        let ry = u32::from(val == 1 || val == 2);
        hilbert_rot(s, &mut x, &mut y, rx, ry);
        x += s * rx;
        y += s * ry;
        d >>= 2;
        s <<= 1;
    }
    (x, y)
}

fn hilbert_rot(n: u32, x: &mut u32, y: &mut u32, rx: u32, ry: u32) {
    if ry == 0 {
        if rx == 1 {
            *x = n - 1 - *x;
            *y = n - 1 - *y;
        }
        std::mem::swap(x, y);
    }
}

// ---------------------------------------------------------------------------
// Decoder surface - STUB pending the elivagar library API.
// ---------------------------------------------------------------------------

/// Memory-mapped PMTiles view. Wraps `elivagar::pmtiles_reader::ArchiveView`.
pub struct ArchiveView {
    _private: (),
}

#[allow(clippy::unused_self, unused_variables, clippy::missing_errors_doc)]
impl ArchiveView {
    /// Open an archive for reading.
    pub fn open(path: &Path) -> io::Result<Self> {
        unimplemented!("elivagar reader API pending: ArchiveView::open (eliv.rs)")
    }
    /// The raw PMTiles header bytes.
    #[must_use]
    pub fn header(&self) -> &[u8] {
        unimplemented!("elivagar reader API pending: ArchiveView::header")
    }
    /// The archive metadata JSON (carries the `elivagar` provenance member).
    pub fn metadata(&self) -> io::Result<String> {
        unimplemented!("elivagar reader API pending: ArchiveView::metadata")
    }
    /// Every directory run, in ascending tile-id order.
    pub fn read_all_runs(&self) -> io::Result<Vec<RawDirEntry>> {
        unimplemented!("elivagar reader API pending: ArchiveView::read_all_runs")
    }
    /// Borrow one stored (still-compressed) blob.
    pub fn raw_blob(&self, blob: BlobRef) -> io::Result<&[u8]> {
        unimplemented!("elivagar reader API pending: ArchiveView::raw_blob")
    }
    #[must_use]
    pub fn tile_type(&self) -> u8 {
        unimplemented!("elivagar reader API pending: ArchiveView::tile_type")
    }
    #[must_use]
    pub fn tile_compression(&self) -> u8 {
        unimplemented!("elivagar reader API pending: ArchiveView::tile_compression")
    }
    #[must_use]
    pub fn min_zoom(&self) -> u8 {
        unimplemented!("elivagar reader API pending: ArchiveView::min_zoom")
    }
    #[must_use]
    pub fn max_zoom(&self) -> u8 {
        unimplemented!("elivagar reader API pending: ArchiveView::max_zoom")
    }
}

/// Reusable scratch buffers for the canonical tile decode. Mirrors elivagar's
/// `DecodeScratch`; kept opaque here.
#[derive(Default)]
pub struct DecodeScratch {
    _private: (),
}

/// Canonical semantic hash of one raw (gzipped MVT) tile blob.
///
/// SCAFFOLD: this decodes through elivagar's MVT decoder and applies brokkr's
/// canonicalization. Both halves are pending the elivagar decoder API; the
/// canonicalization rules will move into `corpus/canonical.rs` when it lands.
#[allow(unused_variables, clippy::missing_errors_doc)]
pub fn semantic_hash(raw: &[u8], scratch: &mut DecodeScratch) -> io::Result<u128> {
    unimplemented!("elivagar decoder API pending: semantic_hash (eliv.rs)")
}

// ---------------------------------------------------------------------------
// Writer surface - STUB pending the elivagar library API. Used only by mutate.
// ---------------------------------------------------------------------------

/// Minimal PMTiles writer config. Mirrors `elivagar::pmtiles_writer::PmtilesConfig`.
#[derive(Clone, Copy, Debug)]
pub struct PmtilesConfig {
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub bounds: (f64, f64, f64, f64),
    pub center: (f64, f64, u8),
}

/// Run-level PMTiles writer plus the one mutate-only verbatim-metadata setter.
pub struct PmtilesWriter {
    _private: (),
}

#[allow(clippy::unused_self, unused_variables, clippy::missing_errors_doc)]
impl PmtilesWriter {
    #[must_use]
    pub fn new(config: PmtilesConfig) -> Self {
        unimplemented!("elivagar writer API pending: PmtilesWriter::new")
    }
    /// The one mutate-only affordance: set the metadata blob verbatim.
    #[allow(clippy::needless_pass_by_value)]
    pub fn set_metadata_verbatim(&mut self, metadata: String) {
        unimplemented!("elivagar writer API pending: set_metadata_verbatim")
    }
    pub fn add_run(&mut self, tile_id: u64, run_length: u32, payload: &[u8]) -> io::Result<()> {
        unimplemented!("elivagar writer API pending: add_run")
    }
    pub fn write_to(self, path: &Path) -> io::Result<()> {
        unimplemented!("elivagar writer API pending: write_to")
    }
}
