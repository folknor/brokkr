//! `brokkr compare-tiles` - the lenient per-layer census of two archives.
//!
//! Native brokkr code over the linked elivagar crate. It used to build and run
//! elivagar's `compare_tiles` cargo example, which carried its own hand-rolled
//! PMTiles reader and MVT scanner; both now come from the crate, so there is one
//! decoder in the system rather than three.
//!
//! This is the **lenient** half of the pair `elivagar.md` describes: where
//! `brokkr regress` diffs two archives feature by feature and returns a verdict,
//! this samples tiles and reports per-layer feature counts side by side. It has
//! no pass/fail, gates nothing, and never refuses - it answers "roughly what
//! changed, and where" for two archives that may not be comparable at all
//! (different producers, different configs, different extracts). That is why it
//! decodes **tolerant**: a foreign producer's extra wire fields are skipped
//! rather than treated as corruption, which is exactly wrong for the gate and
//! exactly right here.
//!
//! One reporting change from the shed example: it counted raw geometry-command
//! varints (`cmds`), a number only reachable by scanning the wire bytes
//! directly. Decoding through `tile_detail` gives vertex counts instead, which
//! is what the column now reports (`verts`) - a better-defined quantity that
//! moves for the same reasons.

use std::collections::BTreeMap;
use std::path::Path;

use super::eliv::{ArchiveView, BlobRef, Strictness, decode_detail_tile, gzip_decompress};
use super::eliv::tile_id_to_zxy;
use crate::error::DevError;
use crate::output;

/// Per-layer accumulator: feature count, the geometry-type split, and vertices.
#[derive(Clone, Copy, Default)]
struct LayerTally {
    features: usize,
    points: usize,
    lines: usize,
    polygons: usize,
    vertices: usize,
}

impl LayerTally {
    fn add(&mut self, other: Self) {
        self.features += other.features;
        self.points += other.points;
        self.lines += other.lines;
        self.polygons += other.polygons;
        self.vertices += other.vertices;
    }
}

type Tallies = BTreeMap<String, LayerTally>;

struct TileEntry {
    tile_id: u64,
    blob: BlobRef,
}

fn addressed_tiles(archive: &ArchiveView) -> Result<Vec<TileEntry>, DevError> {
    let runs = archive
        .read_all_runs()
        .map_err(|e| DevError::Verify(format!("read directory: {e}")))?;
    let mut out = Vec::new();
    for run in runs {
        for step in 0..u64::from(run.run_length) {
            out.push(TileEntry {
                tile_id: run.tile_id + step,
                blob: BlobRef {
                    offset: run.offset,
                    length: run.length,
                },
            });
        }
    }
    Ok(out)
}

fn group_by_zoom(entries: Vec<TileEntry>) -> BTreeMap<u8, Vec<TileEntry>> {
    let mut map: BTreeMap<u8, Vec<TileEntry>> = BTreeMap::new();
    for entry in entries {
        let (z, _, _) = tile_id_to_zxy(entry.tile_id);
        map.entry(z).or_default().push(entry);
    }
    map
}

/// Decode one tile and fold its layers into `into`, returning the decompressed
/// size. A tile that will not decode is skipped rather than fatal: this is a
/// census over a sample, and one bad tile should not deny the other 199.
fn tally_tile(
    archive: &ArchiveView,
    entry: &TileEntry,
    into: &mut Tallies,
) -> Option<usize> {
    let raw = archive.raw_blob(entry.blob).ok()?;
    let bytes = gzip_decompress(raw).ok()?;
    let tile = decode_detail_tile(&bytes, Strictness::Tolerant).ok()?;
    for layer in &tile.layers {
        let tally = into.entry(layer.name.to_string()).or_default();
        for feature in &layer.features {
            tally.features += 1;
            match feature.geom_type {
                1 => tally.points += 1,
                2 => tally.lines += 1,
                3 => tally.polygons += 1,
                _ => {}
            }
            tally.vertices += feature
                .components
                .iter()
                .flat_map(|c| c.rings.iter())
                .map(|r| r.points.len())
                .sum::<usize>();
        }
    }
    Some(bytes.len())
}

fn percent(a: usize, b: usize) -> String {
    if a > 0 && b > 0 {
        #[allow(clippy::cast_precision_loss)]
        let ratio = b as f64 / a as f64;
        format!("{:+.0}%", (ratio - 1.0) * 100.0)
    } else if a == 0 {
        "+inf".to_string()
    } else {
        "-100%".to_string()
    }
}

pub fn run(file_a: &str, file_b: &str, sample: Option<usize>) -> Result<(), DevError> {
    let sample_per_zoom = sample.unwrap_or(200);
    let open = |path: &str| {
        ArchiveView::open(Path::new(path))
            .map_err(|e| DevError::Verify(format!("open {path}: {e}")))
    };
    let name = |path: &str| {
        Path::new(path)
            .file_stem()
            .map_or_else(|| path.to_string(), |s| s.to_string_lossy().to_string())
    };

    let archive_a = open(file_a)?;
    let archive_b = open(file_b)?;
    let name_a = name(file_a);
    let name_b = name(file_b);

    let entries_a = addressed_tiles(&archive_a)?;
    let entries_b = addressed_tiles(&archive_b)?;
    output::bench_msg(&format!(
        "{name_a}: {} tiles, z{}-{}",
        entries_a.len(),
        archive_a.min_zoom(),
        archive_a.max_zoom()
    ));
    output::bench_msg(&format!(
        "{name_b}: {} tiles, z{}-{}",
        entries_b.len(),
        archive_b.min_zoom(),
        archive_b.max_zoom()
    ));

    let min_zoom = archive_a.min_zoom().min(archive_b.min_zoom());
    let max_zoom = archive_a.max_zoom().max(archive_b.max_zoom());
    let by_zoom_a = group_by_zoom(entries_a);
    let by_zoom_b = group_by_zoom(entries_b);

    let mut grand_a: Tallies = BTreeMap::new();
    let mut grand_b: Tallies = BTreeMap::new();

    for z in min_zoom..=max_zoom {
        let empty = Vec::new();
        let (zoom_a, zoom_b) = compare_zoom(
            z,
            (&archive_a, by_zoom_a.get(&z).unwrap_or(&empty)),
            (&archive_b, by_zoom_b.get(&z).unwrap_or(&empty)),
            sample_per_zoom,
        );
        for (layer, tally) in &zoom_a {
            grand_a.entry(layer.clone()).or_default().add(*tally);
        }
        for (layer, tally) in &zoom_b {
            grand_b.entry(layer.clone()).or_default().add(*tally);
        }
    }

    println!("\n=== Grand totals (all sampled tiles) ===\n");
    println!("{:24} {name_a:>10} {name_b:>10} {:>8} {:>10} {:>10}", "layer", "diff", "verts_A", "verts_B");
    println!("{}", "-".repeat(78));
    let mut total_a = LayerTally::default();
    let mut total_b = LayerTally::default();
    for layer in layer_names(&grand_a, &grand_b) {
        let a = grand_a.get(&layer).copied().unwrap_or_default();
        let b = grand_b.get(&layer).copied().unwrap_or_default();
        total_a.add(a);
        total_b.add(b);
        let diff = if a.features > 0 && b.features > 0 {
            percent(a.features, b.features)
        } else if a.features == 0 {
            "only B".to_string()
        } else {
            "only A".to_string()
        };
        println!(
            "{layer:24} {:10} {:10} {diff:>8} {:10} {:10}",
            a.features, b.features, a.vertices, b.vertices
        );
    }
    println!("{}", "-".repeat(78));
    let total_diff = if total_a.features > 0 {
        percent(total_a.features, total_b.features)
    } else {
        "N/A".to_string()
    };
    println!(
        "{:24} {:10} {:10} {total_diff:>8} {:10} {:10}",
        "TOTAL", total_a.features, total_b.features, total_a.vertices, total_b.vertices
    );
    Ok(())
}

/// Sample and tally one zoom level, printing its summary line and per-layer
/// table. Returns the two side's tallies for the grand total.
fn compare_zoom(
    z: u8,
    a: (&ArchiveView, &[TileEntry]),
    b: (&ArchiveView, &[TileEntry]),
    sample_per_zoom: usize,
) -> (Tallies, Tallies) {
    let (archive_a, za) = a;
    let (archive_b, zb) = b;
    let (mut zoom_a, mut zoom_b): (Tallies, Tallies) = (BTreeMap::new(), BTreeMap::new());

    // Only tiles addressed by both sides are comparable.
    let b_index: BTreeMap<u64, usize> =
        zb.iter().enumerate().map(|(i, e)| (e.tile_id, i)).collect();
    let common: Vec<(usize, usize)> = za
        .iter()
        .enumerate()
        .filter_map(|(ai, entry)| b_index.get(&entry.tile_id).map(|&bi| (ai, bi)))
        .collect();
    if common.is_empty() {
        output::bench_msg(&format!(
            "z{z:2}: A={} tiles, B={} tiles, 0 common - skipping",
            za.len(),
            zb.len()
        ));
        return (zoom_a, zoom_b);
    }

    // Even stride rather than the first N, so the sample spans the extent
    // instead of one corner of the Hilbert curve.
    let step = if common.len() <= sample_per_zoom {
        1
    } else {
        common.len() / sample_per_zoom
    };
    let sampled = common.iter().step_by(step).take(sample_per_zoom);

    let (mut bytes_a, mut bytes_b) = (0usize, 0usize);
    let (mut decoded, mut errors) = (0usize, 0usize);
    for &(ai, bi) in sampled {
        match (
            tally_tile(archive_a, &za[ai], &mut zoom_a),
            tally_tile(archive_b, &zb[bi], &mut zoom_b),
        ) {
            (Some(la), Some(lb)) => {
                bytes_a += la;
                bytes_b += lb;
                decoded += 1;
            }
            _ => errors += 1,
        }
    }
    if decoded == 0 {
        output::bench_msg(&format!("z{z:2}: {errors} read errors, 0 decoded - skipping"));
        return (BTreeMap::new(), BTreeMap::new());
    }

    let err_note = if errors > 0 {
        format!(" ({errors} read errors)")
    } else {
        String::new()
    };
    output::bench_msg(&format!(
        "z{z:2}: {decoded} tiles sampled (of {} common, A={} B={} total)  avg_mvt: A={} B={} bytes{err_note}",
        common.len(),
        za.len(),
        zb.len(),
        bytes_a / decoded,
        bytes_b / decoded,
    ));
    print_layers(&zoom_a, &zoom_b, "  ");
    (zoom_a, zoom_b)
}

fn layer_names(a: &Tallies, b: &Tallies) -> Vec<String> {
    let mut names: Vec<String> = a.keys().chain(b.keys()).cloned().collect();
    names.sort();
    names.dedup();
    names
}

fn print_layers(a: &Tallies, b: &Tallies, indent: &str) {
    for layer in layer_names(a, b) {
        let ta = a.get(&layer).copied().unwrap_or_default();
        let tb = b.get(&layer).copied().unwrap_or_default();
        if ta.features == 0 && tb.features == 0 {
            continue;
        }
        // The L/P split is only informative on a mixed-geometry layer.
        let split = |t: LayerTally| {
            if t.lines > 0 || t.polygons > 0 {
                format!(" (L:{} P:{})", t.lines, t.polygons)
            } else {
                String::new()
            }
        };
        println!(
            "{indent}{layer:24} A:{:6}{:16} B:{:6}{:16} verts A:{:8} B:{:8}  {}",
            ta.features,
            split(ta),
            tb.features,
            split(tb),
            ta.vertices,
            tb.vertices,
            percent(ta.features, tb.features)
        );
    }
}
