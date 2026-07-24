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
