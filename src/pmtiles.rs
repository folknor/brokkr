//! PMTiles v3 file statistics.
//!
//! Parses the PMTiles v3 binary format (header, directories, leaf directories)
//! and prints per-zoom tile statistics. Rust rewrite of elivagar's
//! `scripts/pmtiles-stats.py`.

use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::error::DevError;

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

const HEADER_SIZE: usize = 127;
const MAGIC: &[u8; 7] = b"PMTiles";

struct Header {
    root_dir_offset: u64,
    root_dir_length: u64,
    leaf_dirs_offset: u64,
    leaf_dirs_length: u64,
    data_length: u64,
    num_addressed: u64,
    num_entries: u64,
    num_unique: u64,
    internal_compression: u8,
    min_zoom: u8,
    max_zoom: u8,
}

fn parse_header(buf: &[u8; HEADER_SIZE]) -> Result<Header, DevError> {
    if &buf[0..7] != MAGIC || buf[7] != 3 {
        return Err(DevError::Config(format!(
            "not a PMTiles v3 file (magic={:?}, version={})",
            &buf[0..7],
            buf[7],
        )));
    }

    Ok(Header {
        root_dir_offset: u64_le(buf, 8),
        root_dir_length: u64_le(buf, 16),
        // 24: metadata_offset (unused)
        // 32: metadata_length (unused)
        leaf_dirs_offset: u64_le(buf, 40),
        leaf_dirs_length: u64_le(buf, 48),
        // 56: data_offset (unused)
        data_length: u64_le(buf, 64),
        num_addressed: u64_le(buf, 72),
        num_entries: u64_le(buf, 80),
        num_unique: u64_le(buf, 88),
        // 96: tile_type (unused)
        internal_compression: buf[97],
        // 98: tile_compression (unused)
        min_zoom: buf[100],
        max_zoom: buf[101],
    })
}

fn u64_le(buf: &[u8], offset: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&buf[offset..offset + 8]);
    u64::from_le_bytes(bytes)
}

fn i32_le(buf: &[u8], offset: usize) -> i32 {
    let mut bytes = [0u8; 4];
    bytes.copy_from_slice(&buf[offset..offset + 4]);
    i32::from_le_bytes(bytes)
}

// ---------------------------------------------------------------------------
// Bounds (header-only read)
// ---------------------------------------------------------------------------

/// Geographic bounds and zoom range from a PMTiles v3 header.
pub struct TileBounds {
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub min_lon: f64,
    pub min_lat: f64,
    pub max_lon: f64,
    pub max_lat: f64,
}

/// Read geographic bounds and zoom range from a PMTiles v3 header.
pub fn read_bounds(path: &Path) -> Result<TileBounds, DevError> {
    let mut file = File::open(path).map_err(|e| {
        DevError::Config(format!("{}: {e}", path.display()))
    })?;
    let mut buf = [0u8; HEADER_SIZE];
    file.read_exact(&mut buf).map_err(|e| {
        DevError::Config(format!("{}: failed to read header: {e}", path.display()))
    })?;
    let header = parse_header(&buf)?;

    Ok(TileBounds {
        min_zoom: header.min_zoom,
        max_zoom: header.max_zoom,
        min_lon: f64::from(i32_le(&buf, 102)) / 1e7,
        min_lat: f64::from(i32_le(&buf, 106)) / 1e7,
        max_lon: f64::from(i32_le(&buf, 110)) / 1e7,
        max_lat: f64::from(i32_le(&buf, 114)) / 1e7,
    })
}

// ---------------------------------------------------------------------------
// Varint (LEB128 unsigned)
// ---------------------------------------------------------------------------

fn read_varint(data: &[u8], pos: &mut usize) -> u64 {
    let mut result: u64 = 0;
    let mut shift: u32 = 0;
    loop {
        let b = data[*pos];
        *pos += 1;
        result |= u64::from(b & 0x7F) << shift;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    result
}

// ---------------------------------------------------------------------------
// Directory decoding
// ---------------------------------------------------------------------------

struct DirEntry {
    tile_id: u64,
    run_length: u64,
    length: u64,
    offset: u64,
}

fn decode_directory(data: &[u8]) -> Vec<DirEntry> {
    let mut pos = 0;
    #[allow(clippy::cast_possible_truncation)]
    let count = read_varint(data, &mut pos) as usize;

    // Column 1: delta-encoded tile IDs.
    let mut tile_ids = Vec::with_capacity(count);
    let mut prev: u64 = 0;
    for _ in 0..count {
        let delta = read_varint(data, &mut pos);
        prev += delta;
        tile_ids.push(prev);
    }

    // Column 2: run lengths.
    let mut run_lengths = Vec::with_capacity(count);
    for _ in 0..count {
        run_lengths.push(read_varint(data, &mut pos));
    }

    // Column 3: lengths.
    let mut lengths = Vec::with_capacity(count);
    for _ in 0..count {
        lengths.push(read_varint(data, &mut pos));
    }

    // Column 4: offsets (contiguous-tile encoding).
    let mut offsets = Vec::with_capacity(count);
    let mut running: u64 = 0;
    for i in 0..count {
        let val = read_varint(data, &mut pos);
        if val == 0 && i > 0 {
            running += lengths[i - 1];
        } else {
            running = val - 1;
        }
        offsets.push(running);
    }

    let mut entries = Vec::with_capacity(count);
    for i in 0..count {
        entries.push(DirEntry {
            tile_id: tile_ids[i],
            run_length: run_lengths[i],
            length: lengths[i],
            offset: offsets[i],
        });
    }
    entries
}

// ---------------------------------------------------------------------------
// Decompression
// ---------------------------------------------------------------------------

fn compression_name(compression: u8) -> &'static str {
    match compression {
        0 => "unknown",
        1 => "none",
        2 => "gzip",
        3 => "brotli",
        4 => "zstd",
        _ => "unknown",
    }
}

fn decompress(data: &[u8], compression: u8) -> Result<Vec<u8>, DevError> {
    match compression {
        1 => Ok(data.to_vec()),
        2 => {
            let mut decoder = flate2::read::GzDecoder::new(data);
            let mut out = Vec::new();
            decoder.read_to_end(&mut out).map_err(|e| {
                DevError::Config(format!("gzip decompression failed: {e}"))
            })?;
            Ok(out)
        }
        other => Err(DevError::Config(format!(
            "unsupported directory compression: {} ({})",
            compression_name(other),
            other,
        ))),
    }
}

// ---------------------------------------------------------------------------
// Tile ID → zoom level
// ---------------------------------------------------------------------------

fn tile_id_to_zoom(tile_id: u64) -> u8 {
    if tile_id == 0 {
        return 0;
    }
    for z in 1..=31u8 {
        let n: u64 = 1 << z;
        // next_base = (4^(z+1) - 1) / 3
        let next_base = (n * n * 4 - 1) / 3;
        if tile_id < next_base {
            return z;
        }
    }
    31
}

// ---------------------------------------------------------------------------
// Stats collection
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ZoomStats {
    tile_count: u64,
    unique_offsets: HashSet<(u64, u64)>,
    min_length: Option<u64>,
    max_length: u64,
    total_length: u64,
}

impl ZoomStats {
    fn record(&mut self, entry: &DirEntry) {
        let tiles = entry.run_length.max(1);
        self.tile_count += tiles;
        self.unique_offsets.insert((entry.offset, entry.length));

        if entry.length > 0 {
            self.min_length = Some(match self.min_length {
                Some(prev) => prev.min(entry.length),
                None => entry.length,
            });
            self.max_length = self.max_length.max(entry.length);
            self.total_length += entry.length * tiles;
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Read a PMTiles v3 file and print statistics.
#[allow(clippy::too_many_lines)]
pub fn run(path: &str) -> Result<(), DevError> {
    let mut file = File::open(path).map_err(|e| {
        DevError::Config(format!("{path}: {e}"))
    })?;

    // Read header.
    let mut header_buf = [0u8; HEADER_SIZE];
    file.read_exact(&mut header_buf).map_err(|e| {
        DevError::Config(format!("{path}: failed to read header: {e}"))
    })?;

    let header = match parse_header(&header_buf) {
        Ok(h) => h,
        Err(e) => {
            println!("\n=== {path} ===");
            println!("  {e}");
            return Ok(());
        }
    };

    // Read and decompress root directory.
    let root_entries = {
        let compressed = read_range(&mut file, header.root_dir_offset, header.root_dir_length)?;
        let decompressed = decompress(&compressed, header.internal_compression)?;
        decode_directory(&decompressed)
    };

    // Resolve leaf directories if present.
    let all_entries = if header.leaf_dirs_length > 0 {
        let leaf_blob = read_range(&mut file, header.leaf_dirs_offset, header.leaf_dirs_length)?;
        let mut entries = Vec::new();

        for entry in &root_entries {
            if entry.run_length == 0 {
                // Leaf pointer: offset and length index into leaf blob.
                #[allow(clippy::cast_possible_truncation)]
                let start = entry.offset as usize;
                #[allow(clippy::cast_possible_truncation)]
                let end = start + entry.length as usize;
                if end > leaf_blob.len() {
                    return Err(DevError::Config(format!(
                        "{path}: leaf pointer out of bounds (offset={}, length={}, blob={})",
                        entry.offset, entry.length, leaf_blob.len(),
                    )));
                }
                let leaf_data = decompress(&leaf_blob[start..end], header.internal_compression)?;
                entries.extend(decode_directory(&leaf_data));
            } else {
                entries.push(DirEntry {
                    tile_id: entry.tile_id,
                    run_length: entry.run_length,
                    length: entry.length,
                    offset: entry.offset,
                });
            }
        }
        entries
    } else {
        root_entries
    };

    // Compute per-zoom statistics.
    let mut zoom_stats: HashMap<u8, ZoomStats> = HashMap::new();
    for entry in &all_entries {
        let z = tile_id_to_zoom(entry.tile_id);
        zoom_stats.entry(z).or_default().record(entry);
    }

    // Print output.
    let comp = compression_name(header.internal_compression);

    println!("\n=== {path} ===");
    println!(
        "  Zoom: z{}-z{}, internal compression: {comp}",
        header.min_zoom, header.max_zoom,
    );
    println!(
        "  Addressed: {}, Entries: {}, Unique: {}",
        fmt_int(header.num_addressed),
        fmt_int(header.num_entries),
        fmt_int(header.num_unique),
    );
    println!(
        "  Data size: {} bytes ({:.1} MB)",
        fmt_int(header.data_length),
        header.data_length as f64 / 1_048_576.0,
    );
    println!("  Directory entries: {}", fmt_int(all_entries.len() as u64));
    println!("  Per-zoom tiles:");

    let mut zooms: Vec<u8> = zoom_stats.keys().copied().collect();
    zooms.sort_unstable();

    let mut total_tiles: u64 = 0;
    let mut global_min: Option<u64> = None;
    let mut global_max: u64 = 0;
    let mut global_total_length: u64 = 0;

    for &z in &zooms {
        let stats = &zoom_stats[&z];
        total_tiles += stats.tile_count;

        if let Some(min) = stats.min_length {
            global_min = Some(global_min.map_or(min, |prev: u64| prev.min(min)));
        }
        global_max = global_max.max(stats.max_length);
        global_total_length += stats.total_length;

        let avg = match stats.total_length.checked_div(stats.tile_count) {
            Some(v) => format!("{} avg bytes", fmt_int(v)),
            None => String::new(),
        };

        #[allow(clippy::cast_possible_truncation)]
        let unique_count = stats.unique_offsets.len() as u64;
        println!(
            "    z{z:2}: {:>8} tiles, {:>8} unique, {avg:>16}",
            fmt_int(stats.tile_count),
            fmt_int(unique_count),
        );
    }

    println!("  Total: {} tiles", fmt_int(total_tiles));

    if let Some(avg) = global_total_length.checked_div(total_tiles) {
        println!(
            "  Tile sizes: min {} bytes, max {} bytes, avg {} bytes",
            fmt_int(global_min.unwrap_or(0)),
            fmt_int(global_max),
            fmt_int(avg),
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Read `length` bytes from `file` starting at `offset`.
fn read_range(file: &mut File, offset: u64, length: u64) -> Result<Vec<u8>, DevError> {
    file.seek(SeekFrom::Start(offset))?;
    #[allow(clippy::cast_possible_truncation)]
    let mut buf = vec![0u8; length as usize];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

/// Format an integer with comma separators.
fn fmt_int(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let len = bytes.len();
    if len <= 3 {
        return s;
    }

    let mut out = String::with_capacity(len + len / 3);
    let first_group = len % 3;
    if first_group > 0 {
        out.push_str(&s[..first_group]);
    }
    for chunk in s.as_bytes()[first_group..].chunks(3) {
        if !out.is_empty() {
            out.push(',');
        }
        out.push_str(std::str::from_utf8(chunk).unwrap_or("???"));
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- read_varint --------------------------------------------------------

    #[test]
    fn varint_zero() {
        let data = [0x00];
        let mut pos = 0;
        assert_eq!(read_varint(&data, &mut pos), 0);
        assert_eq!(pos, 1);
    }

    #[test]
    fn varint_one() {
        let data = [0x01];
        let mut pos = 0;
        assert_eq!(read_varint(&data, &mut pos), 1);
    }

    #[test]
    fn varint_127() {
        let data = [0x7F];
        let mut pos = 0;
        assert_eq!(read_varint(&data, &mut pos), 127);
    }

    #[test]
    fn varint_128() {
        // 128 = 0x80 → 0x80 0x01
        let data = [0x80, 0x01];
        let mut pos = 0;
        assert_eq!(read_varint(&data, &mut pos), 128);
        assert_eq!(pos, 2);
    }

    #[test]
    fn varint_300() {
        // 300 = 0x12C → low 7 bits = 0x2C | 0x80 = 0xAC, high bits = 0x02
        let data = [0xAC, 0x02];
        let mut pos = 0;
        assert_eq!(read_varint(&data, &mut pos), 300);
    }

    #[test]
    fn varint_16384() {
        // 16384 = 0x4000 → 0x80 0x80 0x01
        let data = [0x80, 0x80, 0x01];
        let mut pos = 0;
        assert_eq!(read_varint(&data, &mut pos), 16384);
        assert_eq!(pos, 3);
    }

    // -- tile_id_to_zoom ----------------------------------------------------

    #[test]
    fn zoom_id_zero() {
        assert_eq!(tile_id_to_zoom(0), 0);
    }

    #[test]
    fn zoom_id_one_through_four_is_z1() {
        // z1 range: tile_ids 1..4 (base for z1 = 1, base for z2 = 5)
        for id in 1..5 {
            assert_eq!(tile_id_to_zoom(id), 1, "tile_id {id} should be z1");
        }
    }

    #[test]
    fn zoom_id_five_is_z2() {
        // z2 base = (4^2 - 1)/3 = 5, z3 base = (4^3 - 1)/3 = 21
        assert_eq!(tile_id_to_zoom(5), 2);
    }

    #[test]
    fn zoom_z2_boundary() {
        // z2 range: 5..20 (z3 base = 21)
        assert_eq!(tile_id_to_zoom(20), 2);
        assert_eq!(tile_id_to_zoom(21), 3);
    }

    // -- decode_directory ---------------------------------------------------

    #[test]
    fn decode_single_entry() {
        // Encode: count=1, tile_id=10, run_length=1, length=100, offset=1 (means 0)
        let mut data = Vec::new();
        push_varint(&mut data, 1); // count
        push_varint(&mut data, 10); // tile_id delta
        push_varint(&mut data, 1); // run_length
        push_varint(&mut data, 100); // length
        push_varint(&mut data, 1); // offset (val-1 = 0)

        let entries = decode_directory(&data);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].tile_id, 10);
        assert_eq!(entries[0].run_length, 1);
        assert_eq!(entries[0].length, 100);
        assert_eq!(entries[0].offset, 0);
    }

    #[test]
    fn decode_delta_tile_ids() {
        let mut data = Vec::new();
        push_varint(&mut data, 3); // count
        // tile_ids: deltas 5, 3, 7 → cumulative 5, 8, 15
        push_varint(&mut data, 5);
        push_varint(&mut data, 3);
        push_varint(&mut data, 7);
        // run_lengths
        push_varint(&mut data, 1);
        push_varint(&mut data, 1);
        push_varint(&mut data, 1);
        // lengths
        push_varint(&mut data, 50);
        push_varint(&mut data, 60);
        push_varint(&mut data, 70);
        // offsets: first=1 (0), second=0 (contiguous: 0+50=50), third=201 (200)
        push_varint(&mut data, 1);
        push_varint(&mut data, 0);
        push_varint(&mut data, 201);

        let entries = decode_directory(&data);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].tile_id, 5);
        assert_eq!(entries[1].tile_id, 8);
        assert_eq!(entries[2].tile_id, 15);
        assert_eq!(entries[0].offset, 0);
        assert_eq!(entries[1].offset, 50, "contiguous: prev offset 0 + prev length 50");
        assert_eq!(entries[2].offset, 200, "explicit: 201 - 1 = 200");
    }

    // -- fmt_int ------------------------------------------------------------

    #[test]
    fn fmt_int_small() {
        assert_eq!(fmt_int(0), "0");
        assert_eq!(fmt_int(999), "999");
    }

    #[test]
    fn fmt_int_thousands() {
        assert_eq!(fmt_int(1_000), "1,000");
        assert_eq!(fmt_int(12_345), "12,345");
        assert_eq!(fmt_int(1_234_567), "1,234,567");
    }

    // -- compression_name ---------------------------------------------------

    #[test]
    fn compression_names() {
        assert_eq!(compression_name(0), "unknown");
        assert_eq!(compression_name(1), "none");
        assert_eq!(compression_name(2), "gzip");
        assert_eq!(compression_name(3), "brotli");
        assert_eq!(compression_name(4), "zstd");
        assert_eq!(compression_name(255), "unknown");
    }

    // -- decompress ---------------------------------------------------------

    #[test]
    fn decompress_none_passthrough() {
        let data = b"hello world";
        let result = decompress(data, 1).unwrap();
        assert_eq!(result, data);
    }

    #[test]
    fn decompress_unsupported_errors() {
        let result = decompress(b"data", 3);
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("brotli"), "error should mention brotli, got: {msg}");
    }

    // -- i32_le -------------------------------------------------------------

    #[test]
    fn i32_le_positive() {
        // Copenhagen longitude 12.57° → 125_700_000 in E7
        let val: i32 = 125_700_000;
        let mut buf = [0u8; 8];
        buf[2..6].copy_from_slice(&val.to_le_bytes());
        assert_eq!(i32_le(&buf, 2), 125_700_000);
    }

    #[test]
    fn i32_le_negative() {
        // Western hemisphere: -73.9° → -739_000_000 in E7
        let val: i32 = -739_000_000;
        let mut buf = [0u8; 4];
        buf.copy_from_slice(&val.to_le_bytes());
        assert_eq!(i32_le(&buf, 0), -739_000_000);
    }

    // -- test helper --------------------------------------------------------

    fn push_varint(buf: &mut Vec<u8>, mut val: u64) {
        loop {
            let mut byte = (val & 0x7F) as u8;
            val >>= 7;
            if val > 0 {
                byte |= 0x80;
            }
            buf.push(byte);
            if val == 0 {
                break;
            }
        }
    }
}
