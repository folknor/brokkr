//! PMTiles statistics and tile-coordinate sampling over elivagar's reader.
//!
//! Header decoding belongs to `PmtilesReader`; this adapter has no independent
//! header-byte offset table. Mutation's write-side header fields are in the
//! shared elivagar constants module.

use std::collections::HashMap;
use std::path::Path;

use crate::elivagar::constants::pmtiles as pmtiles_constants;
use crate::elivagar::eliv::{PmtilesReader, RawDirEntry, tile_id_to_zxy};
use crate::error::DevError;
use crate::output;

struct HeaderStats {
    data_length: u64,
    num_addressed: u64,
    num_entries: u64,
    num_unique: u64,
    internal_compression: u8,
    min_zoom: u8,
    max_zoom: u8,
}

#[derive(Default)]
struct ZoomStats {
    tile_count: u64,
    unique_offsets: std::collections::HashSet<(u64, u32)>,
    min_length: Option<u32>,
    max_length: u32,
    total_length: u64,
}

impl ZoomStats {
    fn record(&mut self, entry: &RawDirEntry) {
        let tiles = u64::from(entry.run_length.max(1));
        self.tile_count = self.tile_count.saturating_add(tiles);
        self.unique_offsets.insert((entry.offset, entry.length));

        if entry.length > 0 {
            self.min_length = Some(
                self.min_length
                    .map_or(entry.length, |prev| prev.min(entry.length)),
            );
            self.max_length = self.max_length.max(entry.length);
            self.total_length = self
                .total_length
                .saturating_add(u64::from(entry.length).saturating_mul(tiles));
        }
    }
}

/// Read a PMTiles archive through elivagar's linked reader and print its
/// directory statistics. Corrupt or unsupported directory compression is an
/// error, never a successful partial report.
#[allow(clippy::too_many_lines)]
pub fn run(path: &str) -> Result<(), DevError> {
    let archive_path = Path::new(path);
    let (header, entries) = read_all_entries(archive_path)?;

    let mut zoom_stats: HashMap<u8, ZoomStats> = HashMap::new();
    for entry in &entries {
        let (zoom, _, _) = tile_id_to_zxy(entry.tile_id);
        zoom_stats.entry(zoom).or_default().record(entry);
    }

    let mut zooms: Vec<u8> = zoom_stats.keys().copied().collect();
    zooms.sort_unstable();

    output::plain(&format!("\n=== {path} ==="));
    output::plain(&format!(
        "  Zoom: z{}-z{}, internal compression: {}",
        header.min_zoom,
        header.max_zoom,
        compression_name(header.internal_compression),
    ));
    output::plain(&format!(
        "  Addressed: {}, Entries: {}, Unique: {}",
        fmt_int(header.num_addressed),
        fmt_int(header.num_entries),
        fmt_int(header.num_unique),
    ));
    output::plain(&format!(
        "  Data size: {} bytes ({:.1} MB)",
        fmt_int(header.data_length),
        header.data_length as f64 / 1_048_576.0,
    ));
    output::plain(&format!(
        "  Directory entries: {}",
        fmt_int(entries.len() as u64)
    ));
    output::plain("  Per-zoom tiles:");

    let mut total_tiles = 0u64;
    let mut global_min: Option<u32> = None;
    let mut global_max = 0u32;
    let mut global_total_length = 0u64;

    for zoom in zooms {
        let stats = &zoom_stats[&zoom];
        total_tiles = total_tiles.saturating_add(stats.tile_count);
        if let Some(min) = stats.min_length {
            global_min = Some(global_min.map_or(min, |prev| prev.min(min)));
        }
        global_max = global_max.max(stats.max_length);
        global_total_length = global_total_length.saturating_add(stats.total_length);

        let average = match stats.total_length.checked_div(stats.tile_count) {
            Some(value) => format!("{} avg bytes", fmt_int(value)),
            None => String::new(),
        };
        output::plain(&format!(
            "    z{zoom:2}: {:>8} tiles, {:>8} unique, {:>16}",
            fmt_int(stats.tile_count),
            fmt_int(stats.unique_offsets.len() as u64),
            average,
        ));
    }

    output::plain(&format!("  Total: {} tiles", fmt_int(total_tiles)));
    if let Some(average) = global_total_length.checked_div(total_tiles) {
        output::plain(&format!(
            "  Tile sizes: min {} bytes, max {} bytes, avg {} bytes",
            fmt_int(u64::from(global_min.unwrap_or(0))),
            fmt_int(u64::from(global_max)),
            fmt_int(average),
        ));
    }
    Ok(())
}

/// Sample up to 20 tile coordinates from the linked reader's directory runs.
/// Zoom levels and coordinates use elivagar's shared PMTiles addressing math.
pub fn sample_tile_coords(path: &Path) -> Result<Vec<(u32, u32, u32)>, DevError> {
    let (_, entries) = read_all_entries(path)?;
    let mut by_zoom: HashMap<u8, Vec<u64>> = HashMap::new();
    for entry in entries {
        if entry.run_length == 0 {
            continue;
        }
        let (zoom, _, _) = tile_id_to_zxy(entry.tile_id);
        by_zoom.entry(zoom).or_default().push(entry.tile_id);
    }

    let mut zooms: Vec<u8> = by_zoom.keys().copied().collect();
    zooms.sort_unstable();
    let selected_zooms = pick_spread(&zooms, 5);
    let mut coords = Vec::new();
    for zoom in selected_zooms {
        if let Some(ids) = by_zoom.get(&zoom) {
            for tile_id in pick_spread(ids, 4) {
                let (_, x, y) = tile_id_to_zxy(tile_id);
                coords.push((u32::from(zoom), x, y));
            }
        }
    }
    Ok(coords)
}

fn read_all_entries(path: &Path) -> Result<(HeaderStats, Vec<RawDirEntry>), DevError> {
    let mut reader = PmtilesReader::open(path).map_err(|error| archive_error(path, &error))?;
    let compression = reader.internal_compression();
    if compression != pmtiles_constants::TILE_COMPRESSION_NONE
        && compression != pmtiles_constants::TILE_COMPRESSION_GZIP
    {
        return Err(DevError::Config(format!(
            "{}: unsupported directory compression: {} ({compression})",
            path.display(),
            compression_name(compression),
        )));
    }
    validate_directory_ranges(&mut reader, path)?;
    let header = HeaderStats {
        data_length: reader.data_length(),
        num_addressed: reader.num_addressed(),
        num_entries: reader.num_entries(),
        num_unique: reader.num_unique(),
        internal_compression: compression,
        min_zoom: reader.min_zoom(),
        max_zoom: reader.max_zoom(),
    };
    let entries = reader
        .read_all_runs()
        .map_err(|error| archive_error(path, &error))?;
    Ok((header, entries))
}

fn validate_directory_ranges(reader: &mut PmtilesReader, path: &Path) -> Result<(), DevError> {
    let file_size = reader
        .file_size()
        .map_err(|error| archive_error(path, &error))?;
    let root_offset = reader.root_dir_offset();
    let root_length = reader.root_dir_length();
    ensure_file_range(path, root_offset, root_length, file_size)?;

    let leaf_offset = reader.leaf_dirs_offset();
    let leaf_length = reader.leaf_dirs_length();
    ensure_file_range(path, leaf_offset, leaf_length, file_size)?;

    let roots = reader
        .read_directory(root_offset, root_length)
        .map_err(|error| archive_error(path, &error))?;
    for entry in roots.iter().filter(|entry| entry.run_length == 0) {
        let length = u64::from(entry.length);
        let leaf_range_valid = entry
            .offset
            .checked_add(length)
            .filter(|end| *end <= leaf_length)
            .is_some();
        if !leaf_range_valid {
            return Err(DevError::Config(format!(
                "{}: leaf directory pointer out of bounds (offset={}, length={}, blob={})",
                path.display(),
                entry.offset,
                length,
                leaf_length,
            )));
        }
        let absolute_offset = leaf_offset.checked_add(entry.offset).ok_or_else(|| {
            DevError::Config(format!(
                "{}: leaf directory offset overflows",
                path.display()
            ))
        })?;
        ensure_file_range(path, absolute_offset, length, file_size)?;
    }
    Ok(())
}

fn ensure_file_range(
    path: &Path,
    offset: u64,
    length: u64,
    file_size: u64,
) -> Result<(), DevError> {
    let in_bounds = offset
        .checked_add(length)
        .is_some_and(|end| end <= file_size);
    if !in_bounds || usize::try_from(length).is_err() {
        return Err(DevError::Config(format!(
            "{}: corrupt PMTiles data: range {offset}+{length} exceeds the file's {file_size} bytes or cannot fit in memory",
            path.display(),
        )));
    }
    Ok(())
}

fn archive_error(path: &Path, error: &std::io::Error) -> DevError {
    DevError::Config(format!("{}: {error}", path.display()))
}

fn compression_name(compression: u8) -> &'static str {
    match compression {
        0 => "unknown",
        pmtiles_constants::TILE_COMPRESSION_NONE => "none",
        pmtiles_constants::TILE_COMPRESSION_GZIP => "gzip",
        pmtiles_constants::TILE_COMPRESSION_BROTLI => "brotli",
        pmtiles_constants::TILE_COMPRESSION_ZSTD => "zstd",
        _ => "unknown",
    }
}

fn pick_spread<T: Copy>(items: &[T], count: usize) -> Vec<T> {
    let len = items.len();
    if len == 0 || count == 0 {
        return Vec::new();
    }
    if count >= len {
        return items.to_vec();
    }
    (0..count)
        .map(|index| items[index * (len - 1) / (count - 1)])
        .collect()
}

fn fmt_int(number: u64) -> String {
    let value = number.to_string();
    let bytes = value.as_bytes();
    let len = bytes.len();
    if len <= 3 {
        return value;
    }

    let mut formatted = String::with_capacity(len + len / 3);
    let first_group = len % 3;
    if first_group > 0 {
        formatted.push_str(&value[..first_group]);
    }
    for (index, digit) in bytes[first_group..].iter().enumerate() {
        if (first_group > 0 && index == 0) || (index > 0 && index % 3 == 0) {
            formatted.push(',');
        }
        formatted.push(char::from(*digit));
    }
    formatted
}
