//! The canonical semantic tile hash - brokkr's per the redesign (`elivagar.md`,
//! "Code that leaves the crate": *the canonical semantic tile hash
//! (`streaming_tile_hash`) and the canonicalization rules*).
//!
//! This is a **verbatim port** of elivagar's frozen `streaming_tile_hash`
//! (pre-redesign `regress.rs`, commit `0129ef3~1`). It is the sole definition of
//! every committed corpus leaf: the committed `corpus/*/leaves` were written by
//! this exact algorithm and this port must reproduce them bit-for-bit (no
//! re-bless - the committed baseline is the parity oracle). The
//! `elivagar-stream-*` domain strings are the version pins: any semantic change
//! here is a corpus-rotation + full-recalibration event, never a quiet edit.
//!
//! It decodes MVT off the wire directly (via `protohoggr`'s cursor and
//! elivagar's shared `decode_detail_attr`), canonicalizing on the fly:
//! unordered collections (layers, features, geometry components) are reduced by
//! hashing their sorted child digests, while rings keep their encoded point
//! order and rotation. The separate `tile_detail` decoder is regress's, not this
//! path's; the equivalence tests keep the two one relation.
//!
//! Casts here are faithful wire-decode arithmetic (zig-zag, signed area) that
//! were `#[allow]`ed at the source; the long match arms are the MVT geometry
//! command grammar.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    clippy::unnecessary_sort_by
)]

use std::io::{self, Read};
use std::sync::Arc;

use flate2::read::GzDecoder;
use protohoggr::{Cursor, WIRE_LEN, WIRE_VARINT};
use xxhash_rust::xxh3::Xxh3;

use super::super::eliv::{CanonRingRole, DetailAttr, Strictness, decode_detail_attr};

// ---------------------------------------------------------------------------
// Public entry: decompress + streaming hash. Signature preserved from the
// elivagar original so `corpus::compute` calls it unchanged.
// ---------------------------------------------------------------------------

/// Reusable decompression buffer, so a full-corpus walk allocates once.
#[derive(Default)]
pub struct DecodeScratch {
    buf: Vec<u8>,
}

impl DecodeScratch {
    fn decompress(&mut self, raw: &[u8]) -> io::Result<&[u8]> {
        self.buf.clear();
        GzDecoder::new(raw).read_to_end(&mut self.buf)?;
        Ok(&self.buf)
    }
}

/// The canonical semantic hash of one raw (gzipped MVT) tile blob.
pub fn semantic_hash(raw: &[u8], scratch: &mut DecodeScratch) -> io::Result<u128> {
    let data = scratch.decompress(raw)?;
    streaming_tile_hash(data).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

// ---------------------------------------------------------------------------
// The detail canonical form: the same "same tile" relation, computed over
// elivagar's wire-order DetailTile instead of the raw MVT stream.
//
// This is the second half of the equivalence relation. The streaming hash above
// decodes MVT inline; this decodes through elivagar's `decode_detail_tile` and
// re-applies the identical canonicalization (stable-sort layers by name,
// multiset features, sort attrs, ordered rings within multiset components) with
// the same `elivagar-stream-*` domains - so the two are value-identical exactly
// when the two independent decode paths agree, which the equivalence tests pin.
//
// It is regress's production "same tile" oracle: `brokkr regress`'s identical-
// tile fast path will call it, giving one source of truth for tile equality.
// (dead_code until that port lands; the equivalence tests exercise it now.)
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub(crate) fn detail_tile_hash(tile: &super::super::eliv::DetailTile) -> u128 {
    let mut layers: Vec<(&str, u128)> = tile
        .layers
        .iter()
        .map(|l| (l.name.as_ref(), detail_layer_hash(l)))
        .collect();
    layers.sort_by(|a, b| a.0.cmp(b.0));
    let mut sink = HashSink::new();
    sink.bytes(b"elivagar-stream-tile-v1");
    write_len(&mut sink, layers.len());
    for (_, hash) in &layers {
        sink.bytes(&hash.to_le_bytes());
    }
    sink.finish()
}

fn detail_layer_hash(layer: &super::super::eliv::DetailLayer) -> u128 {
    let mut feature_hashes: Vec<u128> = layer.features.iter().map(detail_feature_hash).collect();
    let mut sink = HashSink::new();
    sink.bytes(b"elivagar-stream-layer-v2");
    write_string(&mut sink, &layer.name);
    sink.bytes(&layer.extent.to_le_bytes());
    sink.bytes(&layer.version.to_le_bytes());
    sink.bytes(&multiset_hash(b"features", &mut feature_hashes).to_le_bytes());
    sink.finish()
}

fn detail_feature_hash(feature: &super::super::eliv::DetailFeature) -> u128 {
    let mut attrs = feature.attrs.clone();
    attrs.sort();
    let attrs_digest = detail_attrs_hash(&attrs);
    let geometry_digest = detail_geometry_hash(feature.geom_type, &feature.components);
    let mut sink = HashSink::new();
    sink.bytes(b"elivagar-stream-feature-v1");
    match feature.id {
        Some(id) => {
            sink.bytes(&[1]);
            sink.bytes(&id.to_le_bytes());
        }
        None => sink.bytes(&[0]),
    }
    sink.bytes(&[feature.geom_type]);
    sink.bytes(&attrs_digest.to_le_bytes());
    sink.bytes(&geometry_digest.to_le_bytes());
    sink.finish()
}

fn detail_geometry_hash(geom_type: u8, components: &[super::super::eliv::DetailComponent]) -> u128 {
    let mut comp_hashes: Vec<u128> = components.iter().map(detail_component_hash).collect();
    let mut sink = HashSink::new();
    sink.bytes(b"elivagar-stream-geometry-v1");
    sink.bytes(&[geom_type]);
    sink.bytes(&multiset_hash(b"components", &mut comp_hashes).to_le_bytes());
    sink.finish()
}

fn detail_component_hash(component: &super::super::eliv::DetailComponent) -> u128 {
    let mut sink = HashSink::new();
    sink.bytes(b"elivagar-stream-component-v1");
    write_len(&mut sink, component.rings.len());
    for ring in &component.rings {
        sink.bytes(&detail_ring_hash(ring).to_le_bytes());
    }
    sink.finish()
}

/// Regress's canonical feature order: id, then geom type, then attributes, then
/// component geometry (ring count, then role and points per ring). The render
/// core sorts a layer's features through this so the SVG byte order is
/// deterministic and matches the canonical form; regress diffs through it too.
/// Verbatim from elivagar's `compare_detail_features` (needs none of the dropped
/// bbox/structure fields).
#[allow(dead_code)]
pub(crate) fn compare_detail_features(
    a: &super::super::eliv::DetailFeature,
    b: &super::super::eliv::DetailFeature,
) -> std::cmp::Ordering {
    a.id.cmp(&b.id)
        .then_with(|| a.geom_type.cmp(&b.geom_type))
        .then_with(|| a.attrs.cmp(&b.attrs))
        .then_with(|| compare_detail_component_slices(&a.components, &b.components))
}

fn compare_detail_component_slices(
    a: &[super::super::eliv::DetailComponent],
    b: &[super::super::eliv::DetailComponent],
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    for (left, right) in a.iter().zip(b) {
        let rings = left.rings.len().cmp(&right.rings.len());
        if rings != Ordering::Equal {
            return rings;
        }
        for (left_ring, right_ring) in left.rings.iter().zip(&right.rings) {
            let ring = left_ring
                .role
                .cmp(&right_ring.role)
                .then_with(|| left_ring.points.cmp(&right_ring.points));
            if ring != Ordering::Equal {
                return ring;
            }
        }
    }
    a.len().cmp(&b.len())
}

fn detail_ring_hash(ring: &super::super::eliv::DetailRing) -> u128 {
    let mut points = HashSink::new();
    for &(x, y) in &ring.points {
        points.bytes(&x.to_le_bytes());
        points.bytes(&y.to_le_bytes());
    }
    let mut sink = HashSink::new();
    sink.bytes(b"elivagar-stream-ring-v1");
    sink.bytes(&[ring.role as u8]);
    write_len(&mut sink, ring.points.len());
    sink.bytes(&points.finish().to_le_bytes());
    sink.finish()
}

// ---------------------------------------------------------------------------
// Hash sink.
// ---------------------------------------------------------------------------

trait CanonSink {
    fn bytes(&mut self, bytes: &[u8]);
}

struct HashSink(Xxh3);

impl HashSink {
    fn new() -> Self {
        Self(Xxh3::new())
    }
    fn finish(self) -> u128 {
        self.0.digest128()
    }
}

impl CanonSink for HashSink {
    fn bytes(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }
}

fn write_len(sink: &mut impl CanonSink, len: usize) {
    sink.bytes(&u64::try_from(len).unwrap_or(u64::MAX).to_le_bytes());
}

fn write_string(sink: &mut impl CanonSink, value: &str) {
    write_len(sink, value.len());
    sink.bytes(value.as_bytes());
}

fn write_detail_attr(sink: &mut impl CanonSink, value: &DetailAttr) {
    match value {
        DetailAttr::String(value) => {
            sink.bytes(&[1]);
            write_string(sink, value);
        }
        DetailAttr::Float(value) => {
            sink.bytes(&[2]);
            sink.bytes(&value.to_le_bytes());
        }
        DetailAttr::Double(value) => {
            sink.bytes(&[3]);
            sink.bytes(&value.to_le_bytes());
        }
        DetailAttr::Int(value) => {
            sink.bytes(&[4]);
            sink.bytes(&value.to_le_bytes());
        }
        DetailAttr::UInt(value) => {
            sink.bytes(&[5]);
            sink.bytes(&value.to_le_bytes());
        }
        DetailAttr::SInt(value) => {
            sink.bytes(&[6]);
            sink.bytes(&value.to_le_bytes());
        }
        DetailAttr::Bool(value) => sink.bytes(&[7, u8::from(*value)]),
    }
}

fn detail_attrs_hash(attrs: &[(Arc<str>, DetailAttr)]) -> u128 {
    let mut sink = HashSink::new();
    write_len(&mut sink, attrs.len());
    for (key, value) in attrs {
        write_string(&mut sink, key);
        write_detail_attr(&mut sink, value);
    }
    sink.finish()
}

/// Order-insensitive combine: hash the sorted child digests as a sequence.
/// Sorting instead of a wrapping-add sum avoids the additive collision structure
/// of sum-based multiset hashing - equal multisets, and only equal multisets,
/// produce the same sorted sequence.
fn multiset_hash(domain: &[u8], hashes: &mut [u128]) -> u128 {
    hashes.sort_unstable();
    let mut sink = HashSink::new();
    sink.bytes(domain);
    write_len(&mut sink, hashes.len());
    for hash in hashes.iter() {
        sink.bytes(&hash.to_le_bytes());
    }
    sink.finish()
}

// ---------------------------------------------------------------------------
// Feature tag resolution (feature packed tags -> resolved (key, value) attrs).
// ---------------------------------------------------------------------------

fn decode_detail_attrs(
    data: &[u8],
    keys: &[Arc<str>],
    values: &[DetailAttr],
) -> Result<Vec<(Arc<str>, DetailAttr)>, String> {
    let mut cursor = Cursor::new(data);
    let mut attrs = Vec::with_capacity(data.len() / 2);
    while !cursor.is_empty() {
        let key_idx = usize::try_from(
            cursor
                .read_varint()
                .map_err(|error| format!("read feature tag key: {error}"))?,
        )
        .map_err(|_| "feature tag key index overflow".to_string())?;
        let value_idx = usize::try_from(
            cursor
                .read_varint()
                .map_err(|error| format!("read feature tag value: {error}"))?,
        )
        .map_err(|_| "feature tag value index overflow".to_string())?;
        attrs.push((
            Arc::clone(
                keys.get(key_idx)
                    .ok_or_else(|| format!("key index out of range: {key_idx}"))?,
            ),
            values
                .get(value_idx)
                .ok_or_else(|| format!("value index out of range: {value_idx}"))?
                .clone(),
        ));
    }
    Ok(attrs)
}

// ---------------------------------------------------------------------------
// Streaming tile / layer / feature.
// ---------------------------------------------------------------------------

fn streaming_tile_hash(data: &[u8]) -> Result<u128, String> {
    let mut layers = Vec::new();
    let mut cursor = Cursor::new(data);
    while let Some((field, wire_type)) = cursor
        .read_tag()
        .map_err(|error| format!("read tile tag: {error}"))?
    {
        if field == 3 && wire_type == WIRE_LEN {
            layers.push(streaming_layer_hash(
                cursor
                    .read_len_delimited()
                    .map_err(|error| format!("read tile layer: {error}"))?,
            )?);
        } else {
            return Err(format!("unknown tile field {field}"));
        }
    }
    // Mirror the detail decoder's stable sort by name: duplicate layer names
    // keep their encounter order bound, so the two hashes induce the same
    // equivalence relation.
    layers.sort_by(|a, b| a.0.cmp(b.0));
    let mut sink = HashSink::new();
    sink.bytes(b"elivagar-stream-tile-v1");
    write_len(&mut sink, layers.len());
    for (_, hash) in &layers {
        sink.bytes(&hash.to_le_bytes());
    }
    Ok(sink.finish())
}

fn streaming_layer_hash(data: &[u8]) -> Result<(&str, u128), String> {
    let mut name = "";
    let mut extent = 4096u32;
    let mut version = 1u32;
    let mut keys = Vec::new();
    let mut values = Vec::new();
    let mut features = Vec::new();
    let mut cursor = Cursor::new(data);
    while let Some((field, wire_type)) = cursor
        .read_tag()
        .map_err(|error| format!("read layer tag: {error}"))?
    {
        match (field, wire_type) {
            (1, WIRE_LEN) => {
                name = std::str::from_utf8(
                    cursor
                        .read_len_delimited()
                        .map_err(|error| format!("read layer name: {error}"))?,
                )
                .map_err(|error| format!("layer name is not UTF-8: {error}"))?;
            }
            (2, WIRE_LEN) => features.push(
                cursor
                    .read_len_delimited()
                    .map_err(|error| format!("read feature message: {error}"))?,
            ),
            (3, WIRE_LEN) => keys.push(Arc::from(
                std::str::from_utf8(
                    cursor
                        .read_len_delimited()
                        .map_err(|error| format!("read layer key: {error}"))?,
                )
                .map_err(|error| format!("layer key is not UTF-8: {error}"))?,
            )),
            (4, WIRE_LEN) => values.push(decode_detail_attr(
                cursor
                    .read_len_delimited()
                    .map_err(|error| format!("read layer value: {error}"))?,
                Strictness::Strict,
            )?),
            (5, WIRE_VARINT) => {
                let raw = cursor
                    .read_varint()
                    .map_err(|error| format!("read layer extent: {error}"))?;
                extent = u32::try_from(raw).map_err(|_| format!("extent out of range: {raw}"))?;
            }
            (15, WIRE_VARINT) => {
                let raw = cursor
                    .read_varint()
                    .map_err(|error| format!("read layer version: {error}"))?;
                version = u32::try_from(raw).map_err(|_| format!("version out of range: {raw}"))?;
            }
            _ => return Err(format!("unknown layer field {field}")),
        }
    }
    let mut feature_hashes = Vec::with_capacity(features.len());
    for feature in features {
        feature_hashes.push(streaming_feature_hash(feature, &keys, &values)?);
    }
    let mut sink = HashSink::new();
    sink.bytes(b"elivagar-stream-layer-v2");
    write_string(&mut sink, name);
    sink.bytes(&extent.to_le_bytes());
    sink.bytes(&version.to_le_bytes());
    sink.bytes(&multiset_hash(b"features", &mut feature_hashes).to_le_bytes());
    Ok((name, sink.finish()))
}

fn streaming_feature_hash(
    data: &[u8],
    keys: &[Arc<str>],
    values: &[DetailAttr],
) -> Result<u128, String> {
    let mut id = None;
    let mut tag_bytes = Vec::new();
    let mut geom_type = 0u8;
    let mut geometry = None;
    let mut cursor = Cursor::new(data);
    while let Some((field, wire_type)) = cursor
        .read_tag()
        .map_err(|error| format!("read feature tag: {error}"))?
    {
        match (field, wire_type) {
            (1, WIRE_VARINT) => {
                id = Some(
                    cursor
                        .read_varint()
                        .map_err(|error| format!("read feature id: {error}"))?,
                );
            }
            (2, WIRE_LEN) => {
                tag_bytes.extend_from_slice(
                    cursor
                        .read_len_delimited()
                        .map_err(|error| format!("read feature tags: {error}"))?,
                );
            }
            (3, WIRE_VARINT) => {
                let raw = cursor
                    .read_varint()
                    .map_err(|error| format!("read feature type: {error}"))?;
                geom_type =
                    u8::try_from(raw).map_err(|_| format!("geometry type out of range: {raw}"))?;
            }
            (4, WIRE_LEN) => {
                let bytes = cursor
                    .read_len_delimited()
                    .map_err(|error| format!("read feature geometry: {error}"))?;
                geometry.get_or_insert_with(Vec::new).extend_from_slice(bytes);
            }
            _ => return Err(format!("unknown feature field {field}")),
        }
    }
    let mut attrs = decode_detail_attrs(&tag_bytes, keys, values)?;
    attrs.sort();
    let attrs_digest = detail_attrs_hash(&attrs);
    let geometry_digest =
        streaming_geometry_hash(geom_type, geometry.as_deref().unwrap_or_default())?;
    let mut sink = HashSink::new();
    sink.bytes(b"elivagar-stream-feature-v1");
    match id {
        Some(id) => {
            sink.bytes(&[1]);
            sink.bytes(&id.to_le_bytes());
        }
        None => sink.bytes(&[0]),
    }
    sink.bytes(&[geom_type]);
    sink.bytes(&attrs_digest.to_le_bytes());
    sink.bytes(&geometry_digest.to_le_bytes());
    Ok(sink.finish())
}

// ---------------------------------------------------------------------------
// Geometry: rings, components, and the three geometry kinds.
// ---------------------------------------------------------------------------

struct StreamingRing {
    points: HashSink,
    first: Option<(i32, i32)>,
    previous: Option<(i32, i32)>,
    vertices: usize,
    area: i128,
}

impl StreamingRing {
    fn new() -> Self {
        Self {
            points: HashSink::new(),
            first: None,
            previous: None,
            vertices: 0,
            area: 0,
        }
    }

    fn point(&mut self, point: (i32, i32)) {
        self.points.bytes(&point.0.to_le_bytes());
        self.points.bytes(&point.1.to_le_bytes());
        if let Some(previous) = self.previous {
            self.area += i128::from(previous.0) * i128::from(point.1)
                - i128::from(point.0) * i128::from(previous.1);
        } else {
            self.first = Some(point);
        }
        self.previous = Some(point);
        self.vertices += 1;
    }

    fn finish(self, role: CanonRingRole) -> u128 {
        let mut sink = HashSink::new();
        sink.bytes(b"elivagar-stream-ring-v1");
        sink.bytes(&[role as u8]);
        write_len(&mut sink, self.vertices);
        sink.bytes(&self.points.finish().to_le_bytes());
        sink.finish()
    }
}

struct StreamingComponent {
    rings: Vec<u128>,
}

impl StreamingComponent {
    fn new() -> Self {
        Self { rings: Vec::new() }
    }
    fn finish(self) -> u128 {
        let mut sink = HashSink::new();
        sink.bytes(b"elivagar-stream-component-v1");
        write_len(&mut sink, self.rings.len());
        for ring in self.rings {
            sink.bytes(&ring.to_le_bytes());
        }
        sink.finish()
    }
}

fn streaming_geometry_hash(geom_type: u8, data: &[u8]) -> Result<u128, String> {
    let mut components = match geom_type {
        1 => streaming_points(data)?,
        2 => streaming_lines(data)?,
        3 => streaming_polygons(data)?,
        _ => Vec::new(),
    };
    let mut sink = HashSink::new();
    sink.bytes(b"elivagar-stream-geometry-v1");
    sink.bytes(&[geom_type]);
    sink.bytes(&multiset_hash(b"components", &mut components).to_le_bytes());
    Ok(sink.finish())
}

fn unzigzag(value: u32) -> i32 {
    ((value >> 1) as i32) ^ (-((value & 1) as i32))
}

fn read_geometry_varint(cursor: &mut Cursor<'_>, context: &str) -> Result<u32, String> {
    let raw = cursor
        .read_varint()
        .map_err(|error| format!("read {context}: {error}"))?;
    u32::try_from(raw).map_err(|_| format!("{context} out of range: {raw}"))
}

fn next_geometry_point(
    cursor: &mut Cursor<'_>,
    x: &mut i32,
    y: &mut i32,
    context: &str,
) -> Result<(i32, i32), String> {
    *x = x
        .checked_add(unzigzag(read_geometry_varint(cursor, &format!("{context} x"))?))
        .ok_or_else(|| format!("{context} x overflows i32"))?;
    *y = y
        .checked_add(unzigzag(read_geometry_varint(cursor, &format!("{context} y"))?))
        .ok_or_else(|| format!("{context} y overflows i32"))?;
    Ok((*x, *y))
}

fn streaming_points(data: &[u8]) -> Result<Vec<u128>, String> {
    let mut cursor = Cursor::new(data);
    let (mut x, mut y) = (0, 0);
    let mut ring = StreamingRing::new();
    while !cursor.is_empty() {
        let command = read_geometry_varint(&mut cursor, "point command")?;
        if command & 7 != 1 {
            return Err(format!("point geometry contains command {}", command & 7));
        }
        for _ in 0..(command >> 3) {
            ring.point(next_geometry_point(&mut cursor, &mut x, &mut y, "point")?);
        }
    }
    Ok(if ring.vertices == 0 {
        Vec::new()
    } else {
        vec![
            StreamingComponent {
                rings: vec![ring.finish(CanonRingRole::Point)],
            }
            .finish(),
        ]
    })
}

fn streaming_lines(data: &[u8]) -> Result<Vec<u128>, String> {
    let mut cursor = Cursor::new(data);
    let (mut x, mut y) = (0, 0);
    let mut components = Vec::new();
    let mut path: Option<StreamingRing> = None;
    while !cursor.is_empty() {
        let command = read_geometry_varint(&mut cursor, "line command")?;
        match (command & 7, command >> 3) {
            (1, count) => {
                if let Some(path) = path.take() {
                    components.push(
                        StreamingComponent {
                            rings: vec![path.finish(CanonRingRole::Path)],
                        }
                        .finish(),
                    );
                }
                for index in 0..count {
                    let mut ring = StreamingRing::new();
                    ring.point(next_geometry_point(&mut cursor, &mut x, &mut y, "line MoveTo")?);
                    if index == 0 {
                        path = Some(ring);
                    } else {
                        components.push(
                            StreamingComponent {
                                rings: vec![ring.finish(CanonRingRole::Path)],
                            }
                            .finish(),
                        );
                    }
                }
            }
            (2, count) => {
                let path = path
                    .as_mut()
                    .ok_or_else(|| "line LineTo without MoveTo".to_string())?;
                for _ in 0..count {
                    path.point(next_geometry_point(&mut cursor, &mut x, &mut y, "line LineTo")?);
                }
            }
            (7, _) => {}
            (id, _) => return Err(format!("unknown line command {id}")),
        }
    }
    if let Some(path) = path {
        components.push(
            StreamingComponent {
                rings: vec![path.finish(CanonRingRole::Path)],
            }
            .finish(),
        );
    }
    Ok(components)
}

fn streaming_polygons(data: &[u8]) -> Result<Vec<u128>, String> {
    let mut cursor = Cursor::new(data);
    let (mut x, mut y) = (0, 0);
    let mut components = Vec::new();
    let mut component = None;
    let mut ring = None;
    let add_ring = |ring: StreamingRing,
                    component: &mut Option<StreamingComponent>,
                    components: &mut Vec<u128>| {
        let role = if ring.area > 0 {
            CanonRingRole::Outer
        } else {
            CanonRingRole::Hole
        };
        if role == CanonRingRole::Outer || component.is_none() {
            if let Some(component) = component.take() {
                components.push(component.finish());
            }
            *component = Some(StreamingComponent::new());
        }
        component
            .as_mut()
            .expect("component initialized")
            .rings
            .push(ring.finish(role));
    };
    while !cursor.is_empty() {
        let command = read_geometry_varint(&mut cursor, "polygon command")?;
        match (command & 7, command >> 3) {
            (1, count) => {
                for _ in 0..count {
                    if let Some(ring) = ring.take() {
                        add_ring(ring, &mut component, &mut components);
                    }
                    let mut next = StreamingRing::new();
                    next.point(next_geometry_point(&mut cursor, &mut x, &mut y, "polygon MoveTo")?);
                    ring = Some(next);
                }
            }
            (2, count) => {
                let ring = ring
                    .as_mut()
                    .ok_or_else(|| "polygon LineTo without MoveTo".to_string())?;
                for _ in 0..count {
                    ring.point(next_geometry_point(&mut cursor, &mut x, &mut y, "polygon LineTo")?);
                }
            }
            (7, _) => {
                if let Some(ring) = ring.as_mut()
                    && let Some(first) = ring.first
                {
                    ring.point(first);
                }
            }
            (id, _) => return Err(format!("unknown polygon command {id}")),
        }
    }
    if let Some(ring) = ring {
        add_ring(ring, &mut component, &mut components);
    }
    if let Some(component) = component {
        components.push(component.finish());
    }
    Ok(components)
}

// ---------------------------------------------------------------------------
// The streaming <-> detail equivalence tests.
//
// Ported from elivagar's `regress/tests.rs` (0129ef3~1) - the mandatory guard
// that brokkr's two decode paths (the streaming corpus fingerprint above and
// the detail canonical form) remain one equivalence relation, so the digest and
// regress can never silently disagree about "the same tile". Both subjects are
// now brokkr code; elivagar's role is only the wire-order `decode_detail_tile`.
//
// The MVT fixture encoder is ported into brokkr (Option B): `command`/`zigzag`/
// field framing are MVT-spec truths and self-check against the real decoder, and
// several fixtures must build hostile payloads (unknown fields at every message
// level, split packed fields) that a production encoder is designed never to
// emit.
// ---------------------------------------------------------------------------
#[cfg(test)]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::unwrap_used)]
mod equivalence {
    use protohoggr::{
        WIRE_32BIT, WIRE_64BIT, encode_bytes_field_always, encode_tag, encode_varint,
        encode_varint_field_always,
    };

    use elivagar::tile_detail::decode_detail_tile;

    use super::super::super::eliv::Strictness;
    use super::streaming_tile_hash;

    type FeatureSpec<'a> = (Option<u64>, &'a [(i32, i32)]);

    // --- the two hashes over the same tile bytes ---

    fn stream(bytes: &[u8]) -> Result<u128, String> {
        streaming_tile_hash(bytes)
    }
    fn detail(bytes: &[u8]) -> Result<u128, String> {
        Ok(super::detail_tile_hash(&decode_detail_tile(bytes, Strictness::Strict)?))
    }

    /// Both hashes must agree AND deliver the expected verdict: agreement alone
    /// would let correlated mistakes pass.
    fn assert_fingerprint_verdict(left: &[u8], right: &[u8], expect_equal: bool) {
        let detail_equal = detail(left).expect("decode left") == detail(right).expect("decode right");
        let streaming_equal = stream(left).expect("hash left") == stream(right).expect("hash right");
        assert_eq!(detail_equal, expect_equal, "detail verdict");
        assert_eq!(streaming_equal, expect_equal, "streaming verdict");
    }

    // --- minimal MVT fixture encoder (ported from elivagar::mvt) ---

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum GeomType {
        Point = 1,
        LineString = 2,
        Polygon = 3,
    }

    #[derive(Clone)]
    enum Value {
        String(String),
        Float(f32),
        Double(f64),
    }

    fn value_sort_key(v: &Value) -> (u8, u64, String) {
        match v {
            Value::String(s) => (0, 0, s.clone()),
            Value::Float(f) => (1, u64::from(f.to_bits()), String::new()),
            Value::Double(d) => (2, d.to_bits(), String::new()),
        }
    }

    fn value_eq(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::String(x), Value::String(y)) => x == y,
            (Value::Float(x), Value::Float(y)) => x.to_bits() == y.to_bits(),
            (Value::Double(x), Value::Double(y)) => x.to_bits() == y.to_bits(),
            _ => false,
        }
    }

    struct Feature {
        id: Option<u64>,
        geom_type: GeomType,
        geometry: Vec<u32>,
        tags: Vec<(u16, u16)>,
    }

    struct LayerBuilder {
        name: String,
        features: Vec<Feature>,
        keys: Vec<String>,
        values: Vec<Value>,
    }

    impl LayerBuilder {
        fn new(name: &str) -> Self {
            Self { name: name.to_string(), features: Vec::new(), keys: Vec::new(), values: Vec::new() }
        }
        fn intern_key(&mut self, key: &str) -> u16 {
            if let Some(i) = self.keys.iter().position(|k| k == key) {
                return i as u16;
            }
            self.keys.push(key.to_string());
            (self.keys.len() - 1) as u16
        }
        fn intern_value(&mut self, val: Value) -> u16 {
            if let Some(i) = self.values.iter().position(|v| value_eq(v, &val)) {
                return i as u16;
            }
            self.values.push(val);
            (self.values.len() - 1) as u16
        }
        fn add_feature(&mut self, feature: Feature) {
            self.features.push(feature);
        }

        // Faithful port of elivagar::mvt::LayerBuilder::encode: keys sorted
        // alphabetically, values sorted by (type, canonical value), tag indices
        // remapped to the sorted positions.
        fn encode(&self, buf: &mut Vec<u8>) {
            let mut layer = Vec::new();
            let mut sorted_keys: Vec<u16> = (0..self.keys.len() as u16).collect();
            sorted_keys.sort_by(|&a, &b| self.keys[a as usize].cmp(&self.keys[b as usize]));
            let mut key_remap = vec![0u16; self.keys.len()];
            for (new_idx, &old) in sorted_keys.iter().enumerate() {
                key_remap[old as usize] = new_idx as u16;
            }
            let mut sorted_vals: Vec<u16> = (0..self.values.len() as u16).collect();
            sorted_vals.sort_by(|&a, &b| {
                value_sort_key(&self.values[a as usize]).cmp(&value_sort_key(&self.values[b as usize]))
            });
            let mut val_remap = vec![0u16; self.values.len()];
            for (new_idx, &old) in sorted_vals.iter().enumerate() {
                val_remap[old as usize] = new_idx as u16;
            }

            encode_bytes_field_always(&mut layer, 1, self.name.as_bytes());
            for f in &self.features {
                let mut feat = Vec::new();
                if let Some(id) = f.id {
                    encode_varint_field_always(&mut feat, 1, id);
                }
                if !f.tags.is_empty() {
                    let mut packed = Vec::new();
                    for &(k, v) in &f.tags {
                        encode_varint(&mut packed, u64::from(key_remap[k as usize]));
                        encode_varint(&mut packed, u64::from(val_remap[v as usize]));
                    }
                    encode_bytes_field_always(&mut feat, 2, &packed);
                }
                encode_varint_field_always(&mut feat, 3, f.geom_type as u64);
                if !f.geometry.is_empty() {
                    let mut packed = Vec::new();
                    for &g in &f.geometry {
                        encode_varint(&mut packed, u64::from(g));
                    }
                    encode_bytes_field_always(&mut feat, 4, &packed);
                }
                encode_bytes_field_always(&mut layer, 2, &feat);
            }
            for &old in &sorted_keys {
                encode_bytes_field_always(&mut layer, 3, self.keys[old as usize].as_bytes());
            }
            for &old in &sorted_vals {
                let mut val = Vec::new();
                encode_value(&mut val, &self.values[old as usize]);
                encode_bytes_field_always(&mut layer, 4, &val);
            }
            encode_varint_field_always(&mut layer, 5, 4096);
            encode_varint_field_always(&mut layer, 15, 2);
            encode_bytes_field_always(buf, 3, &layer);
        }
    }

    fn encode_value(buf: &mut Vec<u8>, val: &Value) {
        match val {
            Value::String(s) => encode_bytes_field_always(buf, 1, s.as_bytes()),
            Value::Float(f) => {
                encode_tag(buf, 2, WIRE_32BIT);
                buf.extend_from_slice(&f.to_le_bytes());
            }
            Value::Double(d) => {
                encode_tag(buf, 3, WIRE_64BIT);
                buf.extend_from_slice(&d.to_le_bytes());
            }
        }
    }

    fn encode_tile(layers: &[&LayerBuilder]) -> Vec<u8> {
        let mut buf = Vec::new();
        for layer in layers {
            if !layer.features.is_empty() {
                layer.encode(&mut buf);
            }
        }
        buf
    }

    fn command(id: u32, count: u32) -> u32 {
        id | (count << 3)
    }
    fn zigzag(v: i32) -> u32 {
        ((v << 1) ^ (v >> 31)) as u32
    }

    fn encode_linestring(buf: &mut Vec<u32>, coords: &[(i32, i32)]) {
        buf.clear();
        if coords.len() < 2 {
            return;
        }
        buf.push(command(1, 1));
        buf.push(zigzag(coords[0].0));
        buf.push(zigzag(coords[0].1));
        let lineto = buf.len();
        buf.push(0);
        let (mut cx, mut cy) = (coords[0].0, coords[0].1);
        let mut count = 0u32;
        for &(x, y) in &coords[1..] {
            if x == cx && y == cy {
                continue;
            }
            buf.push(zigzag(x - cx));
            buf.push(zigzag(y - cy));
            cx = x;
            cy = y;
            count += 1;
        }
        if count < 1 {
            buf.clear();
            return;
        }
        buf[lineto] = command(2, count);
    }

    fn encode_polygon(buf: &mut Vec<u32>, rings: &[&[(i32, i32)]]) {
        buf.clear();
        let (mut cx, mut cy) = (0i32, 0i32);
        for ring in rings {
            encode_polygon_ring(buf, ring, &mut cx, &mut cy);
        }
    }

    fn encode_polygon_ring(buf: &mut Vec<u32>, ring: &[(i32, i32)], cx: &mut i32, cy: &mut i32) {
        if ring.len() < 4 {
            return;
        }
        let points = &ring[..ring.len() - 1];
        let (save_len, save_cx, save_cy) = (buf.len(), *cx, *cy);
        buf.push(command(1, 1));
        buf.push(zigzag(points[0].0 - *cx));
        buf.push(zigzag(points[0].1 - *cy));
        *cx = points[0].0;
        *cy = points[0].1;
        let lineto = buf.len();
        buf.push(0);
        let mut count = 0u32;
        for &(x, y) in &points[1..] {
            if x == *cx && y == *cy {
                continue;
            }
            buf.push(zigzag(x - *cx));
            buf.push(zigzag(y - *cy));
            *cx = x;
            *cy = y;
            count += 1;
        }
        if count < 2 {
            buf.truncate(save_len);
            *cx = save_cx;
            *cy = save_cy;
            return;
        }
        buf[lineto] = command(2, count);
        buf.push(command(7, 1));
    }

    fn encode_multiline(buf: &mut Vec<u32>, paths: &[&[(i32, i32)]]) {
        buf.clear();
        let (mut cx, mut cy) = (0i32, 0i32);
        for path in paths {
            if path.len() < 2 {
                continue;
            }
            buf.push(command(1, 1));
            buf.push(zigzag(path[0].0 - cx));
            buf.push(zigzag(path[0].1 - cy));
            cx = path[0].0;
            cy = path[0].1;
            let line_count = u32::try_from(path.len() - 1).expect("path length fits u32");
            buf.push(command(2, line_count));
            for &(x, y) in &path[1..] {
                buf.push(zigzag(x - cx));
                buf.push(zigzag(y - cy));
                cx = x;
                cy = y;
            }
        }
    }

    // --- fixtures ---

    fn line_tile(id: Option<u64>, attr: &str, coords: &[(i32, i32)]) -> Vec<u8> {
        let mut layer = LayerBuilder::new("roads");
        let key = layer.intern_key("class");
        let val = layer.intern_value(Value::String(attr.to_string()));
        let mut geom = Vec::new();
        encode_linestring(&mut geom, coords);
        layer.add_feature(Feature { id, geom_type: GeomType::LineString, geometry: geom, tags: vec![(key, val)] });
        encode_tile(&[&layer])
    }

    fn float_attr_tile(value: Value) -> Vec<u8> {
        let mut layer = LayerBuilder::new("attrs");
        let key = layer.intern_key("v");
        let val = layer.intern_value(value);
        let mut geom = Vec::new();
        encode_linestring(&mut geom, &[(0, 0), (10, 10)]);
        layer.add_feature(Feature { id: Some(1), geom_type: GeomType::LineString, geometry: geom, tags: vec![(key, val)] });
        encode_tile(&[&layer])
    }

    fn multiline_tile(paths: &[&[(i32, i32)]]) -> Vec<u8> {
        let mut layer = LayerBuilder::new("roads");
        let key = layer.intern_key("class");
        let val = layer.intern_value(Value::String("path".to_string()));
        let mut geom = Vec::new();
        encode_multiline(&mut geom, paths);
        layer.add_feature(Feature { id: None, geom_type: GeomType::LineString, geometry: geom, tags: vec![(key, val)] });
        encode_tile(&[&layer])
    }

    fn polygon_tile(rings: &[&[(i32, i32)]]) -> Vec<u8> {
        let mut layer = LayerBuilder::new("land");
        let mut geom = Vec::new();
        encode_polygon(&mut geom, rings);
        layer.add_feature(Feature { id: Some(1), geom_type: GeomType::Polygon, geometry: geom, tags: Vec::new() });
        encode_tile(&[&layer])
    }

    fn point_tile(points: &[(i32, i32)]) -> Vec<u8> {
        let mut geometry = Vec::new();
        geometry.push(command(1, u32::try_from(points.len()).expect("point count fits u32")));
        let (mut cx, mut cy) = (0, 0);
        for &(x, y) in points {
            geometry.push(zigzag(x - cx));
            geometry.push(zigzag(y - cy));
            cx = x;
            cy = y;
        }
        raw_geometry_tile(GeomType::Point, geometry)
    }

    fn raw_geometry_tile(geom_type: GeomType, geometry: Vec<u32>) -> Vec<u8> {
        let mut layer = LayerBuilder::new("raw");
        layer.add_feature(Feature { id: Some(1), geom_type, geometry, tags: Vec::new() });
        encode_tile(&[&layer])
    }

    fn two_named_layers_tile(swapped: bool) -> Vec<u8> {
        let mut roads = LayerBuilder::new("roads");
        let mut line = Vec::new();
        encode_linestring(&mut line, &[(0, 0), (10, 10)]);
        roads.add_feature(Feature { id: Some(1), geom_type: GeomType::LineString, geometry: line, tags: Vec::new() });
        let mut land = LayerBuilder::new("land");
        let outer = [(0, 0), (100, 0), (100, 100), (0, 100), (0, 0)];
        let mut poly = Vec::new();
        encode_polygon(&mut poly, &[&outer]);
        land.add_feature(Feature { id: Some(2), geom_type: GeomType::Polygon, geometry: poly, tags: Vec::new() });
        if swapped { encode_tile(&[&land, &roads]) } else { encode_tile(&[&roads, &land]) }
    }

    fn duplicate_name_layers_tile(swapped: bool) -> Vec<u8> {
        let mut a = LayerBuilder::new("roads");
        let mut la = Vec::new();
        encode_linestring(&mut la, &[(0, 0), (10, 10)]);
        a.add_feature(Feature { id: Some(1), geom_type: GeomType::LineString, geometry: la, tags: Vec::new() });
        let mut b = LayerBuilder::new("roads");
        let mut lb = Vec::new();
        encode_linestring(&mut lb, &[(20, 20), (30, 30)]);
        b.add_feature(Feature { id: Some(2), geom_type: GeomType::LineString, geometry: lb, tags: Vec::new() });
        if swapped { encode_tile(&[&b, &a]) } else { encode_tile(&[&a, &b]) }
    }

    fn repeated_feature_tile(paths: &[&[(i32, i32)]]) -> Vec<u8> {
        let mut layer = LayerBuilder::new("roads");
        for path in paths {
            let mut geometry = Vec::new();
            encode_linestring(&mut geometry, path);
            layer.add_feature(Feature { id: None, geom_type: GeomType::LineString, geometry, tags: Vec::new() });
        }
        encode_tile(&[&layer])
    }

    fn versioned_layer_tile(version: Option<u64>) -> Vec<u8> {
        let mut layer = Vec::new();
        encode_bytes_field_always(&mut layer, 1, b"roads");
        encode_varint_field_always(&mut layer, 5, 4096);
        if let Some(v) = version {
            encode_varint_field_always(&mut layer, 15, v);
        }
        let mut tile = Vec::new();
        encode_bytes_field_always(&mut tile, 3, &layer);
        tile
    }

    fn split_geometry_tile(chunks: &[&[u32]]) -> Vec<u8> {
        let mut feature = Vec::new();
        encode_varint_field_always(&mut feature, 1, 1);
        encode_varint_field_always(&mut feature, 3, 2); // LineString
        for chunk in chunks {
            let mut g = Vec::new();
            for &v in *chunk {
                encode_varint(&mut g, u64::from(v));
            }
            encode_bytes_field_always(&mut feature, 4, &g);
        }
        let mut layer = Vec::new();
        encode_bytes_field_always(&mut layer, 1, b"roads");
        encode_varint_field_always(&mut layer, 5, 4096);
        encode_bytes_field_always(&mut layer, 2, &feature);
        let mut tile = Vec::new();
        encode_bytes_field_always(&mut tile, 3, &layer);
        tile
    }

    fn split_tags_tile(chunks: &[&[u32]]) -> Vec<u8> {
        let mut feature = Vec::new();
        encode_varint_field_always(&mut feature, 3, 1); // point
        let mut g = Vec::new();
        for v in [command(1, 1), zigzag(0), zigzag(0)] {
            encode_varint(&mut g, u64::from(v));
        }
        encode_bytes_field_always(&mut feature, 4, &g);
        for chunk in chunks {
            let mut t = Vec::new();
            for &v in *chunk {
                encode_varint(&mut t, u64::from(v));
            }
            encode_bytes_field_always(&mut feature, 2, &t);
        }
        let mut layer = Vec::new();
        encode_bytes_field_always(&mut layer, 1, b"roads");
        encode_varint_field_always(&mut layer, 5, 4096);
        encode_bytes_field_always(&mut layer, 2, &feature);
        encode_bytes_field_always(&mut layer, 3, b"a");
        encode_bytes_field_always(&mut layer, 3, b"b");
        let mut v0 = Vec::new();
        encode_bytes_field_always(&mut v0, 1, b"x");
        encode_bytes_field_always(&mut layer, 4, &v0);
        let mut v1 = Vec::new();
        encode_bytes_field_always(&mut v1, 1, b"y");
        encode_bytes_field_always(&mut layer, 4, &v1);
        let mut tile = Vec::new();
        encode_bytes_field_always(&mut tile, 3, &layer);
        tile
    }

    // --- the equivalence tests ---

    #[test]
    fn streaming_fingerprint_matches_detail_hash_on_geometry() {
        let first = &[(0, 0), (10, 10)][..];
        let second = &[(20, 20), (30, 30)][..];
        // Multiline order across components is erased by both hashes.
        assert_fingerprint_verdict(&multiline_tile(&[first, second]), &multiline_tile(&[second, first]), true);

        let outer = &[(0, 0), (100, 0), (100, 100), (0, 100), (0, 0)][..];
        let rotated = &[(100, 0), (100, 100), (0, 100), (0, 0), (100, 0)][..];
        assert_fingerprint_verdict(&polygon_tile(&[outer]), &polygon_tile(&[rotated]), false);
        assert_fingerprint_verdict(
            &line_tile(Some(1), "a", &[(0, 0), (10, 10)]),
            &line_tile(Some(1), "a", &[(0, 0), (11, 10)]),
            false,
        );

        // Hole order within a component stays semantic in both hashes.
        let hole_a = &[(10, 10), (10, 20), (20, 20), (20, 10), (10, 10)][..];
        let hole_b = &[(30, 30), (30, 40), (40, 40), (40, 30), (30, 30)][..];
        assert_fingerprint_verdict(
            &polygon_tile(&[outer, hole_a, hole_b]),
            &polygon_tile(&[outer, hole_b, hole_a]),
            false,
        );

        // Multipoint order stays semantic in both hashes.
        assert_fingerprint_verdict(&point_tile(&[(0, 0), (10, 10)]), &point_tile(&[(10, 10), (0, 0)]), false);

        // A repeated-count line MoveTo splits identically in both decoders.
        let mut repeated = Vec::new();
        repeated.push(command(1, 2));
        repeated.extend([zigzag(0), zigzag(0), zigzag(5), zigzag(5)]);
        repeated.push(command(2, 1));
        repeated.extend([zigzag(5), zigzag(5)]);
        let mut conventional = Vec::new();
        conventional.push(command(1, 1));
        conventional.extend([zigzag(0), zigzag(0)]);
        conventional.push(command(2, 1));
        conventional.extend([zigzag(10), zigzag(10)]);
        conventional.push(command(1, 1));
        conventional.extend([zigzag(-5), zigzag(-5)]);
        assert_fingerprint_verdict(
            &raw_geometry_tile(GeomType::LineString, repeated),
            &raw_geometry_tile(GeomType::LineString, conventional),
            true,
        );
    }

    #[test]
    fn streaming_fingerprint_matches_detail_hash_on_layers_and_features() {
        let first = &[(0, 0), (10, 10)][..];
        let second = &[(20, 20), (30, 30)][..];

        // Distinct layer names: encoding order is erased by both hashes.
        assert_fingerprint_verdict(&two_named_layers_tile(false), &two_named_layers_tile(true), true);
        // Duplicate layer names: stable sort binds encounter order; swap differs.
        assert_fingerprint_verdict(&duplicate_name_layers_tile(false), &duplicate_name_layers_tile(true), false);
        // Feature multiplicity differs as multisets.
        assert_fingerprint_verdict(
            &repeated_feature_tile(&[first, first, second]),
            &repeated_feature_tile(&[first, second, second]),
            false,
        );
        // Attribute values are bit-exact: float vs double, and signed zeros.
        assert_fingerprint_verdict(&float_attr_tile(Value::Float(1.0)), &float_attr_tile(Value::Double(1.0)), false);
        assert_fingerprint_verdict(&float_attr_tile(Value::Float(0.0)), &float_attr_tile(Value::Float(-0.0)), false);

        // Attribute order within a feature is erased by both hashes.
        fn attrs_tile(reverse: bool) -> Vec<u8> {
            let mut layer = LayerBuilder::new("roads");
            let (class, name, primary, main) = if reverse {
                (
                    layer.intern_key("name"),
                    layer.intern_key("class"),
                    layer.intern_value(Value::String("main".to_string())),
                    layer.intern_value(Value::String("primary".to_string())),
                )
            } else {
                (
                    layer.intern_key("class"),
                    layer.intern_key("name"),
                    layer.intern_value(Value::String("primary".to_string())),
                    layer.intern_value(Value::String("main".to_string())),
                )
            };
            let mut geometry = Vec::new();
            encode_linestring(&mut geometry, &[(0, 0), (10, 10)]);
            layer.add_feature(Feature {
                id: Some(1),
                geom_type: GeomType::LineString,
                geometry,
                tags: vec![(class, primary), (name, main)],
            });
            encode_tile(&[&layer])
        }
        assert_fingerprint_verdict(&attrs_tile(false), &attrs_tile(true), true);

        // Feature order is erased by both hashes.
        fn build(order: [FeatureSpec<'_>; 2]) -> Vec<u8> {
            let mut layer = LayerBuilder::new("roads");
            let key = layer.intern_key("class");
            let value = layer.intern_value(Value::String("path".to_string()));
            for (id, coords) in order {
                let mut geometry = Vec::new();
                encode_linestring(&mut geometry, coords);
                layer.add_feature(Feature { id, geom_type: GeomType::LineString, geometry, tags: vec![(key, value)] });
            }
            encode_tile(&[&layer])
        }
        assert_fingerprint_verdict(
            &build([(Some(1), first), (Some(2), second)]),
            &build([(Some(2), second), (Some(1), first)]),
            true,
        );
    }

    #[test]
    fn layer_version_is_semantic_with_default_one() {
        assert_fingerprint_verdict(&versioned_layer_tile(Some(2)), &versioned_layer_tile(Some(3)), false);
        assert_fingerprint_verdict(&versioned_layer_tile(None), &versioned_layer_tile(Some(1)), true);
        assert_fingerprint_verdict(&versioned_layer_tile(None), &versioned_layer_tile(Some(2)), false);
    }

    #[test]
    fn unknown_fields_are_hard_errors_at_every_message_level() {
        let level_tile = |unknown_at: u8| -> Vec<u8> {
            let mut value = Vec::new();
            encode_bytes_field_always(&mut value, 1, b"x");
            if unknown_at == 3 {
                encode_varint_field_always(&mut value, 9, 1);
            }
            let mut feature = Vec::new();
            encode_varint_field_always(&mut feature, 3, 1);
            if unknown_at == 2 {
                encode_varint_field_always(&mut feature, 9, 1);
            }
            let mut layer = Vec::new();
            encode_bytes_field_always(&mut layer, 1, b"roads");
            encode_varint_field_always(&mut layer, 5, 4096);
            encode_bytes_field_always(&mut layer, 4, &value);
            encode_bytes_field_always(&mut layer, 2, &feature);
            if unknown_at == 1 {
                encode_varint_field_always(&mut layer, 9, 1);
            }
            let mut tile = Vec::new();
            encode_bytes_field_always(&mut tile, 3, &layer);
            if unknown_at == 0 {
                encode_varint_field_always(&mut tile, 9, 1);
            }
            tile
        };
        for level in 0..=3 {
            let tile = level_tile(level);
            assert!(stream(&tile).is_err(), "streaming must reject unknown field at level {level}");
            assert!(
                decode_detail_tile(&tile, Strictness::Strict).is_err(),
                "detail decoder must reject unknown field at level {level}"
            );
        }
    }

    #[test]
    fn packed_fields_split_across_occurrences_hash_identically() {
        let mv = command(1, 1);
        let lt = command(2, 1);
        let z0 = zigzag(0);
        let z10 = zigzag(10);
        let whole_geom = [mv, z0, z0, lt, z10, z10];
        let head = [mv, z0, z0];
        let tail = [lt, z10, z10];
        assert_fingerprint_verdict(
            &split_geometry_tile(&[&whole_geom]),
            &split_geometry_tile(&[&head, &tail]),
            true,
        );
        assert_fingerprint_verdict(&split_tags_tile(&[&[0, 0, 1, 1]]), &split_tags_tile(&[&[0, 0], &[1, 1]]), true);
    }
}
