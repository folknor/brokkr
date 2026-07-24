//! The digest: canonical per-tile leaf runs, the fold into zoom/bucket rollups
//! and roots, the on-disk `digest`/`leaves` text formats, and the baseline
//! self-integrity recomputation.
//!
//! This is pure arithmetic over per-tile hashes plus text (de)serialization -
//! no archive decode - so it is wholly brokkr's (`brokkr.md`, "The fold"). The
//! hash domains and text headers are byte-identical to elivagar's originals so
//! brokkr reads and writes the baselines already committed in elivagar's repo.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::Path;

use xxhash_rust::xxh3::Xxh3;

use super::super::eliv::{tile_id_to_zxy, xy_to_tile_id};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DigestMode {
    Leaves,
    Buckets,
}

#[derive(Clone, Debug)]
pub struct ZoomDigest {
    pub z: u8,
    pub tiles: u64,
    pub hash: u128,
}

#[derive(Clone, Debug)]
pub struct BucketDigest {
    pub z: u8,
    pub cell: u64,
    pub tiles: u64,
    pub hash: u128,
}

#[derive(Clone, Debug)]
pub struct Digest {
    pub mode: DigestMode,
    pub root: u128,
    /// Bucket root: `Some` in `Buckets` mode only - the stronger integrity guard
    /// the opaque bucket rows need.
    pub broot: Option<u128>,
    pub tiles: u64,
    pub entries: u64,
    pub unique: u64,
    pub zooms: Vec<ZoomDigest>,
    pub buckets: Vec<BucketDigest>,
}

/// A maximal same-zoom span of tile ids that share one semantic hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeafRun {
    pub tile_id: u64,
    pub run_length: u32,
    pub hash: u128,
}

// ---------------------------------------------------------------------------
// Hash primitives (domains verbatim from elivagar).
// ---------------------------------------------------------------------------

fn hash(domain: &[u8], items: &[u128]) -> u128 {
    let mut v = items.to_vec();
    v.sort_unstable();
    let mut h = Xxh3::new();
    h.update(domain);
    h.update(&(v.len() as u64).to_le_bytes());
    for x in v {
        h.update(&x.to_le_bytes());
    }
    h.digest128()
}

fn pair(tile: u64, semantic: u128) -> u128 {
    let mut h = Xxh3::new();
    h.update(b"elivagar-corpus-pair-v1");
    h.update(&tile.to_le_bytes());
    h.update(&semantic.to_le_bytes());
    h.digest128()
}

fn root(zooms: &[ZoomDigest]) -> u128 {
    let mut h = Xxh3::new();
    h.update(b"corpus-root-v1");
    h.update(&(zooms.len() as u64).to_le_bytes());
    for z in zooms {
        h.update(&[z.z]);
        h.update(&z.hash.to_le_bytes());
    }
    h.digest128()
}

/// Bucket root over the (z, cell, hash) rows in ascending (z, cell) order.
fn bucket_root(buckets: &[BucketDigest]) -> u128 {
    let mut h = Xxh3::new();
    h.update(b"corpus-bucket-root-v1");
    h.update(&(buckets.len() as u64).to_le_bytes());
    for b in buckets {
        h.update(&[b.z]);
        h.update(&b.cell.to_le_bytes());
        h.update(&b.hash.to_le_bytes());
    }
    h.digest128()
}

/// Fold canonical leaf runs into the full digest. The single definition of every
/// committed count and hash: `compute` (from an archive) and the baseline
/// self-consistency recomputation (from committed `leaves`) both route through
/// it and cannot drift.
#[must_use]
pub fn fold_leaves(leaves: &[LeafRun], mode: DigestMode) -> Digest {
    let mut zoom_pairs: BTreeMap<u8, Vec<u128>> = BTreeMap::new();
    let mut bucket_pairs: BTreeMap<(u8, u64), Vec<u128>> = BTreeMap::new();
    let mut uniques = BTreeSet::new();
    let mut tiles = 0u64;
    for leaf in leaves {
        uniques.insert(leaf.hash);
        for id in leaf.tile_id..leaf.tile_id + u64::from(leaf.run_length) {
            let (z, x, y) = tile_id_to_zxy(id);
            let p = pair(id, leaf.hash);
            zoom_pairs.entry(z).or_default().push(p);
            if mode == DigestMode::Buckets {
                let cell = if z <= 7 {
                    id
                } else {
                    xy_to_tile_id(7, x >> (z - 7), y >> (z - 7))
                };
                bucket_pairs.entry((z, cell)).or_default().push(p);
            }
            tiles += 1;
        }
    }
    let zooms: Vec<_> = zoom_pairs
        .into_iter()
        .map(|(z, v)| ZoomDigest {
            z,
            tiles: v.len() as u64,
            hash: hash(b"corpus-zoom", &v),
        })
        .collect();
    let buckets: Vec<_> = bucket_pairs
        .into_iter()
        .map(|((z, cell), v)| BucketDigest {
            z,
            cell,
            tiles: v.len() as u64,
            hash: hash(b"corpus-bucket", &v),
        })
        .collect();
    let broot = (mode == DigestMode::Buckets).then(|| bucket_root(&buckets));
    Digest {
        mode,
        root: root(&zooms),
        broot,
        tiles,
        entries: leaves.len() as u64,
        unique: uniques.len() as u64,
        zooms,
        buckets,
    }
}

// ---------------------------------------------------------------------------
// Text formats.
// ---------------------------------------------------------------------------

#[must_use]
pub fn hex(v: u128) -> String {
    format!("{v:032x}")
}

fn parse_hex(s: &str) -> io::Result<u128> {
    u128::from_str_radix(s, 16).map_err(|_| invalid("invalid digest hash"))
}

#[must_use]
pub fn digest_text(d: &Digest) -> String {
    let mut out = format!(
        "elivagar-corpus-digest v1\nmode {}\nroot {}\n",
        if d.mode == DigestMode::Leaves {
            "leaves"
        } else {
            "buckets"
        },
        hex(d.root),
    );
    if let Some(broot) = d.broot {
        out.push_str(&format!("broot {}\n", hex(broot)));
    }
    out.push_str(&format!(
        "tiles {} entries {} unique {}\n",
        d.tiles, d.entries, d.unique
    ));
    for z in &d.zooms {
        out.push_str(&format!("zoom {} tiles {} hash {}\n", z.z, z.tiles, hex(z.hash)));
    }
    for b in &d.buckets {
        let (cz, x, y) = tile_id_to_zxy(b.cell);
        out.push_str(&format!(
            "bucket z={} cell={cz}/{x}/{y} tiles={} hash {}\n",
            b.z,
            b.tiles,
            hex(b.hash)
        ));
    }
    out
}

#[must_use]
pub fn leaves_text(leaves: &[LeafRun]) -> String {
    let mut out = "elivagar-corpus-leaves v1\n".to_string();
    for l in leaves {
        let (z, x, y) = tile_id_to_zxy(l.tile_id);
        out.push_str(&format!("{z} {x} {y} {} {}\n", l.run_length, hex(l.hash)));
    }
    out
}

/// Atomically replace `path` with `text` via a sibling temp + rename.
pub fn write_atomic(path: &Path, text: String) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, text)?;
    fs::rename(tmp, path)
}

// ---------------------------------------------------------------------------
// Baseline parse + self-integrity.
// ---------------------------------------------------------------------------

/// The committed digest parsed back in full, so a baseline can be checked for
/// internal consistency before an archive is ever opened.
pub struct Baseline {
    pub mode: DigestMode,
    pub root: u128,
    pub broot: Option<u128>,
    pub tiles: u64,
    pub entries: u64,
    pub unique: u64,
    pub zooms: Vec<ZoomDigest>,
    pub buckets: Vec<BucketDigest>,
}

pub fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

pub fn parse_baseline(path: &Path) -> io::Result<Baseline> {
    parse_baseline_text(&fs::read_to_string(path)?)
}

fn parse_baseline_text(text: &str) -> io::Result<Baseline> {
    let mut mode = None;
    let mut root = None;
    let mut broot = None;
    let mut counts = None;
    let mut zooms = Vec::new();
    let mut buckets = Vec::new();
    for (n, line) in text.lines().enumerate() {
        // The header is mandatory and version-checked, exactly as in
        // `parse_leaves_text`. Accepting a headerless or wrong-version file
        // would let merge damage that truncated the first line - or a future
        // v2 written by a newer brokkr - parse as a v1 baseline.
        if n == 0 {
            if line != "elivagar-corpus-digest v1" {
                return Err(invalid("digest header invalid"));
            }
            continue;
        }
        let p: Vec<&str> = line.split_whitespace().collect();
        match p.first().copied() {
            Some("mode") => {
                mode = match p.get(1) {
                    Some(&"leaves") => Some(DigestMode::Leaves),
                    Some(&"buckets") => Some(DigestMode::Buckets),
                    _ => return Err(invalid("digest mode invalid")),
                };
            }
            Some("root") => {
                root = Some(parse_hex(p.get(1).ok_or_else(|| invalid("root missing hash"))?)?);
            }
            Some("broot") => {
                broot = Some(parse_hex(p.get(1).ok_or_else(|| invalid("broot missing hash"))?)?);
            }
            Some("tiles") => {
                let t = parse_u64(p.get(1).copied())?;
                let e = parse_u64(p.get(3).copied())?;
                let u = parse_u64(p.get(5).copied())?;
                counts = Some((t, e, u));
            }
            Some("zoom") => {
                zooms.push(ZoomDigest {
                    z: parse_u8(p.get(1).copied())?,
                    tiles: parse_u64(p.get(3).copied())?,
                    hash: parse_hex(p.get(5).ok_or_else(|| invalid("zoom missing hash"))?)?,
                });
            }
            Some("bucket") => {
                let z = parse_u8(strip(p.get(1).copied(), "z=").as_deref())?;
                let cell = parse_cell(strip(p.get(2).copied(), "cell=").as_deref())?;
                let tiles = parse_u64(strip(p.get(3).copied(), "tiles=").as_deref())?;
                let hash = parse_hex(p.get(5).ok_or_else(|| invalid("bucket missing hash"))?)?;
                buckets.push(BucketDigest { z, cell, tiles, hash });
            }
            Some("") | None => {}
            Some(other) => return Err(invalid(format!("unrecognized digest line: {other}"))),
        }
    }
    let mode = mode.ok_or_else(|| invalid("digest mode missing"))?;
    let (tiles, entries, unique) = counts.ok_or_else(|| invalid("digest counts missing"))?;
    Ok(Baseline {
        mode,
        root: root.ok_or_else(|| invalid("digest root missing"))?,
        broot,
        tiles,
        entries,
        unique,
        zooms,
        buckets,
    })
}

pub fn parse_leaves(path: &Path) -> io::Result<Vec<LeafRun>> {
    parse_leaves_text(&fs::read_to_string(path)?)
}

fn parse_leaves_text(text: &str) -> io::Result<Vec<LeafRun>> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if n == 0 {
            if line != "elivagar-corpus-leaves v1" {
                return Err(invalid("leaves header invalid"));
            }
            continue;
        }
        if line.is_empty() {
            continue;
        }
        let p: Vec<&str> = line.split_whitespace().collect();
        if p.len() != 5 {
            return Err(invalid("leaves line must be z x y run hash"));
        }
        let z = parse_u8(p.first().copied())?;
        let x = parse_u32(p.get(1).copied())?;
        let y = parse_u32(p.get(2).copied())?;
        let run_length = u32::try_from(parse_u64(p.get(3).copied())?)
            .map_err(|_| invalid("leaf run length out of range"))?;
        let hash = parse_hex(p[4])?;
        out.push(LeafRun {
            tile_id: xy_to_tile_id(z, x, y),
            run_length,
            hash,
        });
    }
    Ok(out)
}

/// Refuse a baseline whose committed rows do not reproduce their own roots.
/// Closes the hand-edit / merge-damage hole, invisible otherwise in the opaque
/// bucket mode. In buckets mode this is a weaker guard (no full leaves exist to
/// recompute from - `brokkr.md`, step 1).
#[must_use]
pub fn baseline_inconsistency(base: &Baseline, leaves: Option<&[LeafRun]>) -> Option<String> {
    if root(&base.zooms) != base.root {
        return Some("root does not match committed zoom rows".into());
    }
    if base.mode == DigestMode::Buckets {
        match base.broot {
            None => return Some("bucket mode is missing broot".into()),
            Some(br) if bucket_root(&base.buckets) != br => {
                return Some("broot does not match committed bucket rows".into());
            }
            Some(_) => {}
        }
    } else if base.broot.is_some() {
        return Some("leaves mode carries an unexpected broot".into());
    }
    if let Some(leaves) = leaves {
        let recomputed = fold_leaves(leaves, base.mode);
        if recomputed.root != base.root {
            return Some("committed leaves do not reproduce root".into());
        }
        if !zooms_equal(&recomputed.zooms, &base.zooms) {
            return Some("committed leaves do not reproduce zoom rows".into());
        }
        if (recomputed.tiles, recomputed.entries, recomputed.unique)
            != (base.tiles, base.entries, base.unique)
        {
            return Some("committed leaves do not reproduce counts".into());
        }
    }
    None
}

fn zooms_equal(a: &[ZoomDigest], b: &[ZoomDigest]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.z == y.z && x.tiles == y.tiles && x.hash == y.hash)
}

// ---------------------------------------------------------------------------
// Small token parsers.
// ---------------------------------------------------------------------------

fn strip(token: Option<&str>, prefix: &str) -> Option<String> {
    token.and_then(|t| t.strip_prefix(prefix)).map(str::to_string)
}
fn parse_u64(token: Option<&str>) -> io::Result<u64> {
    token
        .and_then(|t| t.parse().ok())
        .ok_or_else(|| invalid("expected integer field"))
}
fn parse_u8(token: Option<&str>) -> io::Result<u8> {
    token
        .and_then(|t| t.parse().ok())
        .ok_or_else(|| invalid("expected u8 field"))
}
fn parse_u32(token: Option<&str>) -> io::Result<u32> {
    token
        .and_then(|t| t.parse().ok())
        .ok_or_else(|| invalid("expected coordinate field"))
}
fn parse_cell(token: Option<&str>) -> io::Result<u64> {
    let mut parts = token.ok_or_else(|| invalid("bucket cell missing"))?.split('/');
    let z: u8 = parts
        .next()
        .and_then(|t| t.parse().ok())
        .ok_or_else(|| invalid("cell zoom"))?;
    let x: u32 = parts
        .next()
        .and_then(|t| t.parse().ok())
        .ok_or_else(|| invalid("cell x"))?;
    let y: u32 = parts
        .next()
        .and_then(|t| t.parse().ok())
        .ok_or_else(|| invalid("cell y"))?;
    Ok(xy_to_tile_id(z, x, y))
}

#[cfg(test)]
mod tests {
    //! Ported from elivagar's `corpus::format_tests` - the pure (archive-free)
    //! half that exercises the brokkr-owned fold, text formats, and baseline
    //! self-integrity. The mutation/canonical-run tests wait on the linked
    //! reader/writer (they need real archives).
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn leaves_digest_round_trips_and_self_checks() {
        let leaves = vec![
            LeafRun {
                tile_id: xy_to_tile_id(3, 0, 0),
                run_length: 2,
                hash: 0x1111,
            },
            LeafRun {
                tile_id: xy_to_tile_id(4, 5, 6),
                run_length: 1,
                hash: 0x2222,
            },
        ];
        assert!(leaves[0].tile_id < leaves[1].tile_id);
        let dg = fold_leaves(&leaves, DigestMode::Leaves);
        let base = parse_baseline_text(&digest_text(&dg)).unwrap();
        let parsed = parse_leaves_text(&leaves_text(&leaves)).unwrap();
        assert_eq!(parsed, leaves);
        assert_eq!(base.root, dg.root);
        assert!(base.broot.is_none());
        assert!(baseline_inconsistency(&base, Some(&parsed)).is_none());
    }

    #[test]
    fn bucket_mode_groups_by_z7_ancestor_and_round_trips_broot() {
        let z5 = xy_to_tile_id(5, 3, 4);
        let z8 = xy_to_tile_id(8, 100, 100);
        assert!(z5 < z8);
        let leaves = vec![
            LeafRun {
                tile_id: z5,
                run_length: 1,
                hash: 0xAAAA,
            },
            LeafRun {
                tile_id: z8,
                run_length: 1,
                hash: 0xBBBB,
            },
        ];
        let dg = fold_leaves(&leaves, DigestMode::Buckets);
        assert!(dg.broot.is_some());
        assert!(dg.buckets.iter().any(|b| b.z == 5 && b.cell == z5));
        let ancestor = xy_to_tile_id(7, 100 >> 1, 100 >> 1);
        assert!(dg.buckets.iter().any(|b| b.z == 8 && b.cell == ancestor));
        let base = parse_baseline_text(&digest_text(&dg)).unwrap();
        assert_eq!(base.broot, dg.broot);
        // The z7-vs-z<=7 cell label must round-trip through parse.
        assert!(baseline_inconsistency(&base, None).is_none());
    }

    #[test]
    fn self_consistency_rejects_a_tampered_root() {
        let leaves = vec![LeafRun {
            tile_id: xy_to_tile_id(2, 1, 1),
            run_length: 1,
            hash: 7,
        }];
        let dg = fold_leaves(&leaves, DigestMode::Leaves);
        let text = digest_text(&dg).replace(&hex(dg.root), &format!("{:032x}", dg.root ^ 1));
        let base = parse_baseline_text(&text).unwrap();
        assert!(baseline_inconsistency(&base, Some(&leaves)).is_some());
    }

    #[test]
    fn digest_header_is_mandatory_and_version_checked() {
        let leaves = vec![LeafRun {
            tile_id: xy_to_tile_id(2, 1, 1),
            run_length: 1,
            hash: 5,
        }];
        let text = digest_text(&fold_leaves(&leaves, DigestMode::Leaves));
        assert!(parse_baseline_text(&text).is_ok());
        // Truncated first line (the shape merge damage takes).
        let headerless = text.split_once('\n').unwrap().1.to_string();
        assert!(parse_baseline_text(&headerless).is_err());
        // A version this parser does not speak is not silently read as v1.
        assert!(parse_baseline_text(&text.replace("v1", "v2")).is_err());
    }

    #[test]
    fn leaves_mode_rejects_a_stray_broot() {
        let leaves = vec![LeafRun {
            tile_id: xy_to_tile_id(2, 1, 1),
            run_length: 1,
            hash: 9,
        }];
        let dg = fold_leaves(&leaves, DigestMode::Leaves);
        let mut text = digest_text(&dg);
        text.push_str(&format!("broot {}\n", hex(123)));
        let base = parse_baseline_text(&text).unwrap();
        assert!(baseline_inconsistency(&base, Some(&leaves)).is_some());
    }
}
