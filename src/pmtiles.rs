//! PMTiles statistics and tile-coordinate sampling over elivagar's reader.
//!
//! Header decoding and directory validation belong to elivagar's reader; this
//! adapter reads the typed header and the validated runs.

use std::collections::HashMap;
use std::path::Path;

use crate::elivagar::eliv::{ArchiveView, Header, RawDirEntry, tile_id_to_zxy};
use crate::error::DevError;
use crate::output;

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
        let tiles = u64::from(entry.run_length);
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
        header.min_zoom, header.max_zoom, header.internal_compression,
    ));
    output::plain(&format!(
        "  Addressed: {}, Entries: {}, Unique: {}",
        fmt_int(header.addressed_tiles),
        fmt_int(header.tile_entries),
        fmt_int(header.tile_contents),
    ));
    output::plain(&format!(
        "  Data size: {} bytes ({:.1} MB)",
        fmt_int(header.tile_data.length),
        header.tile_data.length as f64 / 1_048_576.0,
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

/// The header and every run. elivagar's reader refuses an unsupported
/// directory compression and any section, leaf pointer, blob or run outside
/// its bounds.
fn read_all_entries(path: &Path) -> Result<(Header, Vec<RawDirEntry>), DevError> {
    let archive = ArchiveView::open(path).map_err(|error| archive_error(path, &error))?;
    let entries = archive
        .read_all_runs()
        .map_err(|error| archive_error(path, &error))?;
    Ok((*archive.header(), entries))
}

fn archive_error(path: &Path, error: &std::io::Error) -> DevError {
    DevError::Config(format!("{}: {error}", path.display()))
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
