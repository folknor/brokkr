//! The canonical SVG render core, the classifyRings port, and per-tile geometry
//! decode - brokkr's per the redesign. Ported verbatim from elivagar's
//! `corpus/render.rs` (0129ef3~1) over the linked reader/decoder.
//!
//! Cross-host byte identity is a stated obligation (`brokkr.md`, `tiles/*.svg`):
//! integer-only path emission, deterministic iteration order (feature order
//! comes from regress's `compare_detail_features`, layer order from the style),
//! fixed formatting. `check`'s SVG-staleness step byte-compares fresh renders
//! against the committed files, so this must render identically on every host.
//!
//! classifyRings (the MapLibre 500-ring clamp and outer/hole grouping) runs over
//! rings in WIRE order, so geometry is walked verbatim (`wire_feature_paths`)
//! while canonical feature order comes from the detail decode.

use std::cmp::Reverse;
use std::io::{self, Write};
use std::sync::Arc;

use protohoggr::{Cursor, WIRE_LEN, WIRE_VARINT};

use super::super::eliv::{
    ArchiveView, DetailAttr, DetailFeature, Strictness, decode_detail_attr, decode_detail_feature,
    find_entry, gzip_decompress, tile_id_to_zxy, xy_to_tile_id,
};
use super::canonical::{compare_detail_components, compare_detail_features};
use super::style::{Paint, Style};

pub struct RenderTile {
    pub layers: Vec<RenderLayer>,
}
pub struct RenderLayer {
    pub name: Arc<str>,
    pub extent: u32,
    pub features: Vec<RenderFeature>,
}
type WirePaths = Vec<Vec<(i32, i32)>>;

pub struct RenderFeature {
    pub geom_type: u8,
    pub(crate) attrs: Vec<(Arc<str>, DetailAttr)>,
    pub wire_paths: WirePaths,
}

/// A rendered tile: the SVG bytes plus any non-fatal render warnings.
pub struct RenderedSvg {
    pub bytes: Vec<u8>,
    pub warnings: Vec<String>,
}

pub fn decode_render_tile(data: &[u8]) -> Result<RenderTile, String> {
    let mut layers = Vec::new();
    let mut cursor = Cursor::new(data);
    while let Some((field, wire)) = cursor.read_tag().map_err(|e| e.to_string())? {
        if field != 3 || wire != WIRE_LEN {
            return Err("invalid MVT tile".into());
        }
        let layer = cursor.read_len_delimited().map_err(|e| e.to_string())?;
        layers.push(decode_render_layer(layer)?);
    }
    Ok(RenderTile { layers })
}

// Decode one layer into canonical-order features carrying WIRE-order geometry.
fn decode_render_layer(data: &[u8]) -> Result<RenderLayer, String> {
    let (name, extent, keys, values, feature_bytes) = wire_layer(data)?;
    let mut items: Vec<(DetailFeature, WirePaths)> = Vec::with_capacity(feature_bytes.len());
    for bytes in &feature_bytes {
        let mut detail = decode_detail_feature(bytes, &keys, &values, Strictness::Strict)?;
        // The wire-order decoder leaves attrs/components unsorted; the canonical
        // feature order (and the committed SVG feature order) is defined over the
        // sorted form, so canonicalize before comparing. Geometry is still
        // emitted from the WIRE-order `wire_paths` (classifyRings needs it).
        detail.attrs.sort();
        detail.components.sort_by(compare_detail_components);
        let (_, wire_paths) = wire_feature_paths(bytes)?;
        items.push((detail, wire_paths));
    }
    items.sort_by(|a, b| compare_detail_features(&a.0, &b.0));
    let features = items
        .into_iter()
        .map(|(detail, wire_paths)| RenderFeature {
            geom_type: detail.geom_type,
            attrs: detail.attrs,
            wire_paths,
        })
        .collect();
    Ok(RenderLayer {
        name: Arc::from(name),
        extent,
        features,
    })
}

#[must_use]
pub fn classify_rings(rings: &[Vec<(i32, i32)>]) -> (Vec<Vec<usize>>, u32) {
    if rings.len() <= 1 {
        return (
            if rings.is_empty() { Vec::new() } else { vec![vec![0]] },
            0,
        );
    }
    let indexed: Vec<(usize, i128)> = rings
        .iter()
        .enumerate()
        .map(|(i, r)| (i, area(r)))
        .filter(|(_, a)| *a != 0)
        .collect();
    let Some((_, first)) = indexed.first().copied() else {
        return (Vec::new(), 0);
    };
    let outer = first > 0;
    let mut groups = Vec::new();
    for (i, signed) in indexed {
        if (signed > 0) == outer || groups.is_empty() {
            groups.push(vec![i]);
        } else if let Some(group) = groups.last_mut() {
            group.push(i);
        }
    }
    let mut clamped = 0u32;
    for group in &mut groups {
        if group.len() > 500 {
            let before = group.len();
            group.sort_by_key(|&index| Reverse(area(&rings[index]).unsigned_abs()));
            group.truncate(500);
            clamped = clamped.saturating_add(u32::try_from(before - 500).unwrap_or(u32::MAX));
        }
    }
    (groups, clamped)
}

fn area(ring: &[(i32, i32)]) -> i128 {
    if ring.is_empty() {
        return 0;
    }
    ring.iter()
        .enumerate()
        .map(|(i, p)| {
            let q = ring[(i + ring.len() - 1) % ring.len()];
            i128::from(q.0 - p.0) * i128::from(q.1 + p.1)
        })
        .sum()
}

pub(crate) fn path_data(paths: &[&[(i32, i32)]], close: bool) -> String {
    let mut out = String::new();
    for path in paths {
        if let Some((x, y)) = path.first() {
            out.push_str(&format!("M{x} {y}"));
            for (x, y) in &path[1..] {
                out.push_str(&format!(" L{x} {y}"));
            }
            if close {
                out.push_str(" Z");
            }
        }
    }
    out
}

pub fn render_svg(
    tile: &RenderTile,
    z: u8,
    x: u32,
    y: u32,
    style: &Style,
    layers: Option<&[String]>,
) -> Result<RenderedSvg, String> {
    let mut bytes = format!("<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 4096 4096\" width=\"512\" height=\"512\">\n  <title>{z}/{x}/{y}</title>\n  <rect width=\"4096\" height=\"4096\" fill=\"{}\"/>\n", xml_attr(&style.file.background)).into_bytes();
    let mut refs: Vec<&RenderLayer> = tile
        .layers
        .iter()
        .filter(|l| layers.is_none_or(|wanted| wanted.iter().any(|name| name == l.name.as_ref())))
        .collect();
    refs.sort_by_key(|l| (style.position(&l.name).unwrap_or(usize::MAX), l.name.as_ref()));
    let mut warnings = Vec::new();
    for layer in refs {
        if layer.extent == 0 || 4096 % layer.extent != 0 {
            return Err(format!(
                "{z}/{x}/{y} layer {} has non-integral extent {}",
                layer.name, layer.extent
            ));
        }
        if style.position(&layer.name).is_none() {
            warnings.push(format!("unstyled layer {}", layer.name));
        }
        let scale = 4096 / layer.extent;
        bytes.extend_from_slice(
            format!(
                "  <g id=\"{}\"{}>\n",
                xml_attr(&layer.name),
                if scale == 1 {
                    String::new()
                } else {
                    format!(" transform=\"scale({scale})\"")
                }
            )
            .as_bytes(),
        );
        for (i, f) in layer.features.iter().enumerate() {
            let paint = style.resolve(&layer.name, &f.attrs);
            emit_feature(&mut bytes, &layer.name, i, f, &paint);
        }
        bytes.extend_from_slice(b"  </g>\n");
    }
    bytes.extend_from_slice(b"</svg>\n");
    Ok(RenderedSvg { bytes, warnings })
}

#[allow(clippy::too_many_lines)]
fn emit_feature(out: &mut Vec<u8>, layer: &str, i: usize, f: &RenderFeature, paint: &Paint) {
    let id = xml_attr(&format!("{layer}-f{i}"));
    match f.geom_type {
        3 => {
            let (groups, clamped) = classify_rings(&f.wire_paths);
            for (j, g) in groups.iter().enumerate() {
                let paths: Vec<_> = g.iter().map(|&n| f.wire_paths[n].as_slice()).collect();
                let mut s = format!(
                    "    <path id=\"{}-p{j}\" d=\"{}\" fill=\"{}\" fill-rule=\"nonzero\"",
                    id,
                    xml_attr(&path_data(&paths, true)),
                    xml_attr(paint.fill.as_deref().unwrap_or("#ff00ff"))
                );
                if let Some(v) = &paint.fill_opacity {
                    s.push_str(&format!(" fill-opacity=\"{}\"", xml_attr(v)));
                }
                if let Some(v) = &paint.stroke {
                    s.push_str(&format!(" stroke=\"{}\"", xml_attr(v)));
                }
                if let Some(v) = &paint.stroke_width {
                    s.push_str(&format!(" stroke-width=\"{}\"", xml_attr(v)));
                }
                if clamped > 0 {
                    s.push_str(&format!(" data-clamped=\"{clamped}\""));
                }
                s.push_str("/>\n");
                out.extend_from_slice(s.as_bytes());
            }
        }
        2 => {
            let paths: Vec<_> = f.wire_paths.iter().map(Vec::as_slice).collect();
            let mut s = format!(
                "    <path id=\"{id}\" d=\"{}\" fill=\"none\" stroke=\"{}\" stroke-width=\"{}\"",
                xml_attr(&path_data(&paths, false)),
                xml_attr(paint.stroke.as_deref().or(paint.fill.as_deref()).unwrap_or("#ff00ff")),
                xml_attr(paint.stroke_width.as_deref().unwrap_or("1"))
            );
            if let Some(v) = &paint.stroke_dasharray {
                s.push_str(&format!(" stroke-dasharray=\"{}\"", xml_attr(v)));
            }
            if let Some(v) = &paint.stroke_opacity {
                s.push_str(&format!(" stroke-opacity=\"{}\"", xml_attr(v)));
            }
            s.push_str("/>\n");
            out.extend_from_slice(s.as_bytes());
        }
        1 => {
            for (k, path) in f.wire_paths.iter().enumerate() {
                for (x, y) in path {
                    out.extend_from_slice(format!("    <circle id=\"{id}-p{k}\" cx=\"{x}\" cy=\"{y}\" r=\"{}\" fill=\"{}\"/>\n", paint.point_radius.unwrap_or(4), xml_attr(paint.fill.as_deref().or(paint.stroke.as_deref()).unwrap_or("#ff00ff"))).as_bytes());
                }
            }
        }
        _ => {}
    }
}

pub fn render_archive_tile(
    archive: &ArchiveView,
    z: u8,
    x: u32,
    y: u32,
    style: &Style,
    layers: Option<&[String]>,
) -> io::Result<RenderedSvg> {
    let runs = archive.read_all_runs()?;
    let id = xy_to_tile_id(z, x, y);
    let raw = if let Some(e) = find_entry(&runs, id) {
        gzip_decompress(archive.raw_blob_at(e.offset, e.length)?)?
    } else {
        Vec::new()
    };
    let tile = decode_render_tile(&raw).map_err(io::Error::other)?;
    render_svg(&tile, z, x, y, style, layers).map_err(io::Error::other)
}

/// Emit the ring-grouping report - the input to the ring-grouping differential
/// oracle. One line per polygon feature: `z/x/y layer i groups`.
pub fn dump_ring_grouping(archive: &ArchiveView, out: &mut dyn Write) -> io::Result<()> {
    let mut lines = Vec::new();
    for run in archive.read_all_runs()? {
        let raw = gzip_decompress(archive.raw_blob_at(run.offset, run.length)?)?;
        for tile_id in run.tile_id..run.tile_id + u64::from(run.run_length) {
            let (z, x, y) = tile_id_to_zxy(tile_id);
            let mut tile = Cursor::new(&raw);
            while let Some((field, wire)) = tile.read_tag().map_err(io::Error::other)? {
                if field != 3 || wire != WIRE_LEN {
                    return Err(io::Error::other("invalid MVT tile"));
                }
                let layer = tile.read_len_delimited().map_err(io::Error::other)?;
                let (name, _extent, _keys, _values, features) =
                    wire_layer(layer).map_err(io::Error::other)?;
                for (i, feature) in features.iter().enumerate() {
                    let (typ, rings) = wire_feature_paths(feature).map_err(io::Error::other)?;
                    if typ != 3 {
                        continue;
                    }
                    let (g, _) = classify_rings(&rings);
                    let group = if g.is_empty() {
                        "-".into()
                    } else {
                        g.iter()
                            .map(|v| {
                                v.iter()
                                    .map(|&n| rings[n].len().to_string())
                                    .collect::<Vec<_>>()
                                    .join("+")
                            })
                            .collect::<Vec<_>>()
                            .join("|")
                    };
                    lines.push(format!("{z}/{x}/{y} {name} {i} {group}"));
                }
            }
        }
    }
    lines.sort();
    for line in lines {
        writeln!(out, "{line}")?;
    }
    Ok(())
}

#[allow(clippy::type_complexity)]
fn wire_layer(
    data: &[u8],
) -> Result<(String, u32, Vec<Arc<str>>, Vec<DetailAttr>, Vec<Vec<u8>>), String> {
    let mut c = Cursor::new(data);
    let mut name = String::new();
    let mut extent = 4096u32;
    let mut keys: Vec<Arc<str>> = Vec::new();
    let mut values = Vec::new();
    let mut features = Vec::new();
    while let Some((f, w)) = c.read_tag().map_err(|e| e.to_string())? {
        match (f, w) {
            (1, WIRE_LEN) => {
                name = std::str::from_utf8(c.read_len_delimited().map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?
                    .into();
            }
            (2, WIRE_LEN) => {
                features.push(c.read_len_delimited().map_err(|e| e.to_string())?.to_vec());
            }
            (3, WIRE_LEN) => keys.push(Arc::from(
                std::str::from_utf8(c.read_len_delimited().map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?,
            )),
            (4, WIRE_LEN) => values.push(decode_detail_attr(
                c.read_len_delimited().map_err(|e| e.to_string())?,
                Strictness::Strict,
            )?),
            (5, WIRE_VARINT) => {
                extent = u32::try_from(c.read_varint().map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
            }
            (15, WIRE_VARINT) => {
                c.read_varint().map_err(|e| e.to_string())?;
            }
            _ => return Err("unknown layer field".into()),
        }
    }
    Ok((name, extent, keys, values, features))
}

// Walk a feature message into (geom_type, paths) with rings/paths/points in
// WIRE order.
#[allow(clippy::type_complexity)]
fn wire_feature_paths(data: &[u8]) -> Result<(u8, Vec<Vec<(i32, i32)>>), String> {
    let mut c = Cursor::new(data);
    let (mut typ, mut geom) = (0u64, Vec::new());
    while let Some((f, w)) = c.read_tag().map_err(|e| e.to_string())? {
        match (f, w) {
            (1 | 3, WIRE_VARINT) => {
                let value = c.read_varint().map_err(|e| e.to_string())?;
                if f == 3 {
                    typ = value;
                }
            }
            (2 | 4, WIRE_LEN) => {
                let value = c.read_len_delimited().map_err(|e| e.to_string())?;
                if f == 4 {
                    geom.extend_from_slice(value);
                }
            }
            _ => return Err("unknown feature field".into()),
        }
    }
    let geom_type = u8::try_from(typ).map_err(|_| "geometry type out of range")?;
    let mut c = Cursor::new(&geom);
    let (mut x, mut y) = (0i32, 0i32);
    let mut paths: Vec<Vec<(i32, i32)>> = Vec::new();
    while !c.is_empty() {
        let command = u32::try_from(c.read_varint().map_err(|e| e.to_string())?)
            .map_err(|_| "geometry command overflow")?;
        match command & 7 {
            1 => {
                for _ in 0..command >> 3 {
                    x = x
                        .checked_add(unzigzag(
                            u32::try_from(c.read_varint().map_err(|e| e.to_string())?)
                                .map_err(|_| "x overflow")?,
                        ))
                        .ok_or("x overflow")?;
                    y = y
                        .checked_add(unzigzag(
                            u32::try_from(c.read_varint().map_err(|e| e.to_string())?)
                                .map_err(|_| "y overflow")?,
                        ))
                        .ok_or("y overflow")?;
                    paths.push(vec![(x, y)]);
                }
            }
            2 => {
                let path = paths.last_mut().ok_or("LineTo without MoveTo")?;
                for _ in 0..command >> 3 {
                    x = x
                        .checked_add(unzigzag(
                            u32::try_from(c.read_varint().map_err(|e| e.to_string())?)
                                .map_err(|_| "x overflow")?,
                        ))
                        .ok_or("x overflow")?;
                    y = y
                        .checked_add(unzigzag(
                            u32::try_from(c.read_varint().map_err(|e| e.to_string())?)
                                .map_err(|_| "y overflow")?,
                        ))
                        .ok_or("y overflow")?;
                    path.push((x, y));
                }
            }
            7 => {
                if geom_type == 3
                    && let Some(r) = paths.last_mut()
                    && let Some(first) = r.first().copied()
                {
                    r.push(first);
                }
            }
            _ => return Err("unknown geometry command".into()),
        }
    }
    Ok((geom_type, paths))
}

#[allow(clippy::cast_possible_wrap)]
fn unzigzag(value: u32) -> i32 {
    ((value >> 1) as i32) ^ (-((value & 1) as i32))
}

#[must_use]
pub fn xml_text(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
#[must_use]
pub fn xml_attr(value: &str) -> String {
    xml_text(value).replace('"', "&quot;").replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::{classify_rings, xml_attr, xml_text};

    fn pos(a: i32, b: i32) -> Vec<(i32, i32)> {
        vec![(a, b), (a + 1, b), (a + 1, b + 1), (a, b + 1), (a, b)]
    }
    fn neg(a: i32, b: i32) -> Vec<(i32, i32)> {
        vec![(a, b), (a, b + 1), (a + 1, b + 1), (a + 1, b), (a, b)]
    }

    #[test]
    fn classify_rings_keeps_single_zero_area_ring() {
        let rings = vec![vec![(0, 0), (0, 0), (0, 0)]];
        assert_eq!(classify_rings(&rings), (vec![vec![0]], 0));
    }

    #[test]
    fn classify_rings_skips_zero_area_and_attaches_hole() {
        let rings = vec![
            vec![(0, 0), (10, 0), (10, 10), (0, 0)],
            vec![(0, 0), (0, 0)],
            vec![(2, 2), (2, 8), (8, 8), (2, 2)],
        ];
        assert_eq!(classify_rings(&rings).0, vec![vec![0, 2]]);
    }

    #[test]
    fn classify_rings_two_outers_are_two_polygons() {
        let rings = vec![pos(0, 0), pos(100, 100)];
        assert_eq!(classify_rings(&rings), (vec![vec![0], vec![1]], 0));
    }

    #[test]
    fn classify_rings_calibrates_off_first_ring() {
        let rings = vec![neg(0, 0), pos(100, 100)];
        assert_eq!(classify_rings(&rings).0, vec![vec![0, 1]]);
    }

    #[test]
    fn classify_rings_clamps_at_500_with_ties() {
        let mut rings = vec![vec![(0, 0), (4096, 0), (4096, 4096), (0, 4096), (0, 0)]];
        for i in 0..500 {
            rings.push(neg(i * 2, 0));
        }
        let (groups, clamped) = classify_rings(&rings);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 500);
        assert_eq!(clamped, 1);
    }

    #[test]
    fn xml_escaping_neutralizes_metacharacters() {
        assert_eq!(xml_text("a&b<c>d"), "a&amp;b&lt;c&gt;d");
        assert_eq!(xml_attr("\"x'&\""), "&quot;x&apos;&amp;&quot;");
        assert!(!xml_attr("\"/><script>").contains('<'));
        assert!(!xml_attr("\"/><script>").contains('"'));
    }
}
