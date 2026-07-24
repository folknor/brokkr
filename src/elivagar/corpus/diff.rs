//! The localized content diff: which tiles/zooms/buckets changed between the
//! committed baseline and a fresh archive. Pure over the parsed rows.

use std::collections::{BTreeMap, BTreeSet};

use super::super::eliv::tile_id_to_zxy;
use super::digest::{BucketDigest, LeafRun, ZoomDigest, hex};

const DIFF_CAP: usize = 100;

#[derive(Default)]
struct LeafDiff {
    // index 0 changed, 1 added, 2 removed
    counts: [u64; 3],
    lines: [Vec<String>; 3],
    cur: Option<(usize, u64, u64, u128, u128)>,
}
impl LeafDiff {
    fn push(&mut self, class: usize, id: u64, oldh: u128, newh: u128) {
        if let Some((c, start, len, o, nh)) = self.cur.as_mut()
            && *c == class
            && *o == oldh
            && *nh == newh
            && id == *start + *len
            && tile_id_to_zxy(*start).0 == tile_id_to_zxy(id).0
        {
            *len += 1;
            return;
        }
        self.flush();
        self.cur = Some((class, id, 1, oldh, newh));
    }
    fn flush(&mut self) {
        let Some((class, start, len, oldh, newh)) = self.cur.take() else {
            return;
        };
        self.counts[class] += 1;
        if self.lines[class].len() < DIFF_CAP {
            let (z, x, y) = tile_id_to_zxy(start);
            let line = match class {
                0 => format!("changed {z} {x} {y} {len} {}->{}", hex(oldh), hex(newh)),
                1 => format!("added {z} {x} {y} {len} {}", hex(newh)),
                _ => format!("removed {z} {x} {y} {len} {}", hex(oldh)),
            };
            self.lines[class].push(line);
        }
    }
}

/// Per-tile diff of two leaf-run sets. Returns (changed-run count, capped lines).
#[must_use]
pub fn leaf_diff(committed: &[LeafRun], current: &[LeafRun]) -> (u64, Vec<String>) {
    let old = expand(committed);
    let new = expand(current);
    let mut st = LeafDiff::default();
    let (mut i, mut j) = (0usize, 0usize);
    while i < old.len() || j < new.len() {
        let take_old = j >= new.len() || (i < old.len() && old[i].0 <= new[j].0);
        let take_new = i >= old.len() || (j < new.len() && new[j].0 <= old[i].0);
        if take_old && take_new {
            let (id, oh) = old[i];
            let nh = new[j].1;
            if oh != nh {
                st.push(0, id, oh, nh);
            }
            i += 1;
            j += 1;
        } else if take_old {
            let (id, oh) = old[i];
            st.push(2, id, oh, 0);
            i += 1;
        } else {
            let (id, nh) = new[j];
            st.push(1, id, 0, nh);
            j += 1;
        }
    }
    st.flush();
    let mut out = Vec::new();
    for (class, label) in [(0, "changed"), (1, "added"), (2, "removed")] {
        out.extend(st.lines[class].iter().cloned());
        let extra = st.counts[class] - st.lines[class].len() as u64;
        if extra > 0 {
            out.push(format!("(+{extra} more {label} runs)"));
        }
    }
    (st.counts.iter().sum(), out)
}

fn expand(leaves: &[LeafRun]) -> Vec<(u64, u128)> {
    let mut out = Vec::new();
    for l in leaves {
        for id in l.tile_id..l.tile_id + u64::from(l.run_length) {
            out.push((id, l.hash));
        }
    }
    out
}

#[must_use]
pub fn zoom_delta_lines(committed: &[ZoomDigest], current: &[ZoomDigest]) -> Vec<String> {
    let cmap: BTreeMap<u8, &ZoomDigest> = committed.iter().map(|z| (z.z, z)).collect();
    let nmap: BTreeMap<u8, &ZoomDigest> = current.iter().map(|z| (z.z, z)).collect();
    let mut zs: BTreeSet<u8> = BTreeSet::new();
    zs.extend(cmap.keys().copied());
    zs.extend(nmap.keys().copied());
    let mut out = Vec::new();
    for z in zs {
        let ct = cmap.get(&z).map_or(0, |z| z.tiles);
        let nt = nmap.get(&z).map_or(0, |z| z.tiles);
        let hash_changed = match (cmap.get(&z), nmap.get(&z)) {
            (Some(a), Some(b)) => a.hash != b.hash,
            _ => true,
        };
        if ct != nt || hash_changed {
            let flag = if hash_changed { " hash-changed" } else { "" };
            out.push(format!("zoom {z} tiles {ct}->{nt}{flag}"));
        }
    }
    out
}

#[must_use]
pub fn bucket_delta_lines(committed: &[BucketDigest], current: &[BucketDigest]) -> (u64, Vec<String>) {
    let cmap: BTreeMap<(u8, u64), &BucketDigest> =
        committed.iter().map(|b| ((b.z, b.cell), b)).collect();
    let nmap: BTreeMap<(u8, u64), &BucketDigest> =
        current.iter().map(|b| ((b.z, b.cell), b)).collect();
    let mut keys: BTreeSet<(u8, u64)> = BTreeSet::new();
    keys.extend(cmap.keys().copied());
    keys.extend(nmap.keys().copied());
    let mut out = Vec::new();
    let mut changed = 0u64;
    for key @ (z, _cell) in keys {
        let ct = cmap.get(&key).map_or(0, |b| b.tiles);
        let nt = nmap.get(&key).map_or(0, |b| b.tiles);
        let hash_changed = match (cmap.get(&key), nmap.get(&key)) {
            (Some(a), Some(b)) => a.hash != b.hash,
            _ => true,
        };
        if ct != nt || hash_changed {
            changed += 1;
            if out.len() < DIFF_CAP {
                let (cz, x, y) = tile_id_to_zxy(key.1);
                out.push(format!("bucket z={z} cell={cz}/{x}/{y} tiles {ct}->{nt}"));
            }
        }
    }
    if changed > out.len() as u64 {
        out.push(format!("(+{} more changed buckets)", changed - out.len() as u64));
    }
    (changed, out)
}
