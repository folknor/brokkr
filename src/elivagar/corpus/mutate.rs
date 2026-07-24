//! `mutate` - the calibration instrument. Rewrites an archive with one
//! controlled mutation while preserving its PMTiles header configuration and
//! metadata byte-for-byte after decompression. Not a production tile-editing
//! API; its sole consumer is the calibration suite (`brokkr.md`, Calibration).
//!
//! Brokkr-owned adjudication tooling, ported over elivagar's writer surface
//! (`eliv::PmtilesWriter`, incl. the one mutate-only `set_metadata_verbatim`).
//! The protobuf field surgery below is self-contained (no elivagar decoder).

use std::io::{self, Read, Write};
use std::path::Path;

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;

use super::super::eliv::{
    ArchiveView, BlobRef, PmtilesConfig, PmtilesWriter, RawDirEntry, read_i32_le, xy_to_tile_id,
};

/// A deliberately small set of direct archive mutations used to calibrate the
/// corpus gate. `drop-tile`/`nudge-geometry`/`layer-version` must FIRE with the
/// target named; `regzip` (byte-different, semantically equal) must CLEAR.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationOp {
    DropTile,
    NudgeGeometry,
    LayerVersion,
    Regzip,
}

impl MutationOp {
    /// Parse the CLI `--op` value.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "drop-tile" => Self::DropTile,
            "nudge-geometry" => Self::NudgeGeometry,
            "layer-version" => Self::LayerVersion,
            "regzip" => Self::Regzip,
            _ => return None,
        })
    }
}

fn encode_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            out.push(byte | 0x80);
        } else {
            out.push(byte);
            break;
        }
    }
}

/// Rewrite `input` into `output` applying one mutation, header + metadata
/// preserved verbatim.
pub fn mutate(
    input: &Path,
    output: &Path,
    target: Option<(u8, u32, u32)>,
    op: MutationOp,
) -> io::Result<()> {
    if op != MutationOp::Regzip && target.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--tile is required for this mutation",
        ));
    }
    let archive = ArchiveView::open(input)?;
    let target_id = target.map(|(z, x, y)| xy_to_tile_id(z, x, y));
    let mut found = op == MutationOp::Regzip;
    let header = archive.header();
    let e7 = |offset| f64::from(read_i32_le(header, offset)) / 10_000_000.0;
    let config = PmtilesConfig {
        min_zoom: archive.min_zoom(),
        max_zoom: archive.max_zoom(),
        bounds: (e7(102), e7(106), e7(110), e7(114)),
        center: (e7(119), e7(123), header[118]),
    };
    let mut writer = PmtilesWriter::new(config);
    writer.set_metadata_verbatim(archive.metadata()?);
    for run in archive.read_all_runs()? {
        let is_target = op != MutationOp::Regzip
            && target_id
                .is_some_and(|id| id >= run.tile_id && id < run.tile_id + u64::from(run.run_length));
        if is_target {
            found = true;
            let id = target_id.expect("checked above");
            copy_run(&mut writer, &archive, run, run.tile_id, id - run.tile_id)?;
            match op {
                MutationOp::DropTile => {}
                MutationOp::NudgeGeometry | MutationOp::LayerVersion => {
                    let raw = archive.raw_blob(BlobRef {
                        offset: run.offset,
                        length: run.length,
                    })?;
                    let edited = mutate_payload(raw, op)?;
                    writer.add_run(id, 1, &edited)?;
                }
                MutationOp::Regzip => unreachable!(),
            }
            let after = id + 1;
            copy_run(
                &mut writer,
                &archive,
                run,
                after,
                run.tile_id + u64::from(run.run_length) - after,
            )?;
        } else if op == MutationOp::Regzip {
            let raw = archive.raw_blob(BlobRef {
                offset: run.offset,
                length: run.length,
            })?;
            let recompressed = regzip(raw)?;
            writer.add_run(run.tile_id, run.run_length, &recompressed)?;
        } else {
            copy_run(&mut writer, &archive, run, run.tile_id, u64::from(run.run_length))?;
        }
    }
    if !found {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "target tile is not addressed by archive",
        ));
    }
    writer.write_to(output)
}

fn copy_run(
    writer: &mut PmtilesWriter,
    archive: &ArchiveView,
    run: RawDirEntry,
    tile_id: u64,
    length: u64,
) -> io::Result<()> {
    if length != 0 {
        let length =
            u32::try_from(length).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "run too long"))?;
        writer.add_run(
            tile_id,
            length,
            archive.raw_blob(BlobRef {
                offset: run.offset,
                length: run.length,
            })?,
        )?;
    }
    Ok(())
}

fn regzip(raw: &[u8]) -> io::Result<Vec<u8>> {
    let mut decoded = Vec::new();
    GzDecoder::new(raw).read_to_end(&mut decoded)?;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::new(9));
    encoder.write_all(&decoded)?;
    encoder.finish()
}

#[derive(Clone)]
struct Field {
    number: u32,
    wire: u8,
    value: Vec<u8>,
}

fn read_varint(input: &[u8], at: &mut usize) -> io::Result<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *input
            .get(*at)
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "truncated protobuf varint"))?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(io::Error::new(io::ErrorKind::InvalidData, "protobuf varint overflows u64"))
}

fn fields(input: &[u8]) -> io::Result<Vec<Field>> {
    let mut at = 0;
    let mut out = Vec::new();
    while at < input.len() {
        let tag = read_varint(input, &mut at)?;
        let number = u32::try_from(tag >> 3)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "field number too large"))?;
        let wire = u8::try_from(tag & 7).unwrap_or(u8::MAX);
        let value = match wire {
            0 => {
                let start = at;
                read_varint(input, &mut at)?;
                input[start..at].to_vec()
            }
            1 => take(input, &mut at, 8)?,
            2 => {
                let n = usize::try_from(read_varint(input, &mut at)?)
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "length too large"))?;
                take(input, &mut at, n)?
            }
            5 => take(input, &mut at, 4)?,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unsupported protobuf wire type {wire}"),
                ));
            }
        };
        out.push(Field { number, wire, value });
    }
    Ok(out)
}

fn take(input: &[u8], at: &mut usize, n: usize) -> io::Result<Vec<u8>> {
    let end = at
        .checked_add(n)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "protobuf length overflows"))?;
    let value = input
        .get(*at..end)
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "truncated protobuf field"))?
        .to_vec();
    *at = end;
    Ok(value)
}

fn encode_fields(fields: &[Field]) -> Vec<u8> {
    let mut out = Vec::new();
    for field in fields {
        encode_varint(&mut out, u64::from(field.number) << 3 | u64::from(field.wire));
        if field.wire == 2 {
            encode_varint(&mut out, field.value.len() as u64);
        }
        out.extend_from_slice(&field.value);
    }
    out
}

fn mutate_payload(raw: &[u8], op: MutationOp) -> io::Result<Vec<u8>> {
    let mut decoded = Vec::new();
    GzDecoder::new(raw).read_to_end(&mut decoded)?;
    let mut tile = fields(&decoded)?;
    let layer = tile
        .iter_mut()
        .find(|f| f.number == 3 && f.wire == 2)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "tile has no layer"))?;
    let mut layer_fields = fields(&layer.value)?;
    match op {
        MutationOp::LayerVersion => {
            if let Some(version) = layer_fields.iter_mut().find(|f| f.number == 15 && f.wire == 0) {
                version.value = vec![3];
            } else {
                layer_fields.push(Field {
                    number: 15,
                    wire: 0,
                    value: vec![3],
                });
            }
        }
        MutationOp::NudgeGeometry => {
            let feature_index = layer_fields
                .iter()
                .enumerate()
                .find_map(|(index, field)| {
                    (field.number == 2
                        && field.wire == 2
                        && fields(&field.value)
                            .ok()?
                            .iter()
                            .any(|nested| nested.number == 4 && nested.wire == 2))
                    .then_some(index)
                })
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "layer has no feature geometry")
                })?;
            let feature = &mut layer_fields[feature_index];
            let mut feature_fields = fields(&feature.value)?;
            let geometry = feature_fields
                .iter_mut()
                .find(|f| f.number == 4 && f.wire == 2)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "feature has no geometry"))?;
            geometry.value = nudge_geometry(&geometry.value)?;
            feature.value = encode_fields(&feature_fields);
        }
        _ => unreachable!(),
    }
    layer.value = encode_fields(&layer_fields);
    let mut encoder = GzEncoder::new(Vec::new(), Compression::new(6));
    encoder.write_all(&encode_fields(&tile))?;
    encoder.finish()
}

fn nudge_geometry(input: &[u8]) -> io::Result<Vec<u8>> {
    let mut at = 0;
    while at < input.len() {
        let start = at;
        let command = read_varint(input, &mut at)?;
        if command & 7 == 1 && command >> 3 != 0 {
            let parameter = read_varint(input, &mut at)?;
            let magnitude = i64::try_from(parameter >> 1)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "geometry delta too large"))?;
            let delta = if parameter & 1 == 0 {
                magnitude
            } else {
                -magnitude - 1
            };
            let changed_delta = delta
                .checked_add(1)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "geometry delta overflow"))?;
            let changed = if changed_delta >= 0 {
                u64::try_from(changed_delta).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "geometry delta conversion failed")
                })? * 2
            } else {
                changed_delta.unsigned_abs() * 2 - 1
            };
            let mut out = input[..start].to_vec();
            encode_varint(&mut out, command);
            encode_varint(&mut out, changed);
            out.extend_from_slice(&input[at..]);
            return Ok(out);
        }
        let pairs = usize::try_from(command >> 3)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "geometry count too large"))?;
        for _ in 0..pairs.saturating_mul(2) {
            read_varint(input, &mut at)?;
        }
    }
    Err(io::Error::new(io::ErrorKind::InvalidData, "geometry has no MoveTo"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::let_underscore_must_use)]
mod calibration {
    //! The mutate calibrands (ported from elivagar's `corpus::mutation_tests`):
    //! `drop-tile`/`nudge-geometry`/`layer-version` must change the target
    //! tile's canonical hash and nothing else, and `regzip` (byte-different)
    //! must be semantically neutral. Built over the real `PmtilesWriter` +
    //! canonical hash on a synthetic archive - no external data - so they run in
    //! the normal test suite (`brokkr.md`, Calibration).

    use std::collections::BTreeMap;
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};

    use flate2::Compression;
    use flate2::write::GzEncoder;

    use super::super::super::eliv::{
        ArchiveView, PmtilesConfig, PmtilesWriter, tile_id_to_zxy, xy_to_tile_id,
    };
    use super::super::digest::{DigestMode, LeafRun};
    use super::super::compute;
    use super::{Field, MutationOp, encode_fields, mutate};

    fn fixture_payload() -> Vec<u8> {
        // One point feature: geometry MoveTo(1) at (0,0); extent 4096, version 2.
        let feature = encode_fields(&[
            Field { number: 3, wire: 0, value: vec![1] },
            Field { number: 4, wire: 2, value: vec![9, 0, 0] },
        ]);
        let layer = encode_fields(&[
            Field { number: 1, wire: 2, value: b"test".to_vec() },
            Field { number: 2, wire: 2, value: feature },
            Field { number: 5, wire: 0, value: vec![0x80, 0x20] },
            Field { number: 15, wire: 0, value: vec![2] },
        ]);
        let tile = encode_fields(&[Field { number: 3, wire: 2, value: layer }]);
        let mut gzip = GzEncoder::new(Vec::new(), Compression::new(6));
        gzip.write_all(&tile).unwrap();
        gzip.finish().unwrap()
    }

    fn test_dir() -> PathBuf {
        let path = PathBuf::from("target").join(format!("corpus-mutation-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    // A z1 run of 4 tiles sharing one blob; the target is the second tile.
    fn source(path: &Path) -> u64 {
        let payload = fixture_payload();
        let mut writer = PmtilesWriter::new(PmtilesConfig {
            min_zoom: 1,
            max_zoom: 1,
            bounds: (0.0, 0.0, 1.0, 1.0),
            center: (0.0, 0.0, 1),
        });
        let first = xy_to_tile_id(1, 0, 0);
        writer.add_run(first, 4, &payload).unwrap();
        writer.write_to(path).unwrap();
        first + 1
    }

    fn leaf_hashes(leaves: &[LeafRun]) -> BTreeMap<u64, u128> {
        leaves
            .iter()
            .flat_map(|leaf| {
                (leaf.tile_id..leaf.tile_id + u64::from(leaf.run_length)).map(move |id| (id, leaf.hash))
            })
            .collect()
    }

    #[test]
    fn mutations_are_isolated_and_regzip_is_semantically_neutral() {
        let dir = test_dir();
        let input = dir.join("source.pmtiles");
        let target = source(&input);
        let before = ArchiveView::open(&input).unwrap();
        let (before_digest, before_leaves) = compute(&before, DigestMode::Leaves).unwrap();
        let before_meta = before.metadata().unwrap();

        for (op, name) in [
            (MutationOp::DropTile, "drop"),
            (MutationOp::NudgeGeometry, "nudge"),
            (MutationOp::LayerVersion, "version"),
        ] {
            let output = dir.join(format!("{name}.pmtiles"));
            mutate(&input, &output, Some(tile_id_to_zxy(target)), op).unwrap();
            let after = ArchiveView::open(&output).unwrap();
            assert_eq!(after.metadata().unwrap(), before_meta, "{name} metadata changed");
            let (digest, leaves) = compute(&after, DigestMode::Leaves).unwrap();
            assert_ne!(digest.root, before_digest.root, "{name} did not FIRE (digest unchanged)");
            let old = leaf_hashes(&before_leaves);
            let new = leaf_hashes(&leaves);
            for (&id, &hash) in &old {
                if id != target {
                    assert_eq!(new.get(&id), Some(&hash), "{name} changed non-target tile {id}");
                }
            }
        }

        // regzip: byte-different, semantically identical (must CLEAR).
        let output = dir.join("regzip.pmtiles");
        mutate(&input, &output, None, MutationOp::Regzip).unwrap();
        let after = ArchiveView::open(&output).unwrap();
        let (digest, leaves) = compute(&after, DigestMode::Leaves).unwrap();
        assert_eq!(after.metadata().unwrap(), before_meta);
        assert_eq!(digest.root, before_digest.root, "regzip changed the digest");
        assert_eq!(leaves, before_leaves, "regzip changed the leaves");
        assert_ne!(fs::read(&input).unwrap(), fs::read(&output).unwrap(), "regzip left bytes unchanged");

        let _ = fs::remove_dir_all(&dir);
    }
}
