//! A header-only walk of a PBF's blob framing and its blob-level indexdata.
//!
//! `verify getid-removeid` picks the IDs it requests from the dataset under
//! test, and at planet scale it may not read the dataset to do so; `verify
//! check-ids` reads from the same walk the kind claims pbfhogg judges type
//! order from ([`framing`]). This walk
//! reads each blob's `BlobHeader` (a few dozen bytes) and seeks past the
//! payload without reading it, so its I/O is proportional to the blob count,
//! never to the data. The one payload it reads is the leading `OSMHeader`'s,
//! for the header's required features.
//!
//! The indexdata decoder follows pbfhogg's wire format (its `blob_meta`
//! module): a version byte (1 with exactly 26 bytes, 2 or 3 with exactly 42),
//! the element kind ordinal (node 0, way 1, relation 2), the minimum and
//! maximum ID as little-endian `i64` at offsets 2 and 10, and the element
//! count as little-endian `u64` at 18. It refuses exactly the claims pbfhogg
//! cannot read - an unknown version, a length the version does not declare,
//! an unknown kind, an inverted range, or two non-empty indexdata fields on
//! one header - so a file this walk accepts is one pbfhogg reads on its
//! indexdata path. An unknown future version is a loud refusal naming it.
//!
//! The walk assumes the file is not modified in place while it runs: one
//! open handle serves the whole walk, and its length is read once.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::error::DevError;

/// pbfhogg refuses a `BlobHeader` length of zero or of 64 KiB and above.
const MAX_HEADER_LEN: u32 = 64 * 1024;
/// pbfhogg refuses a blob payload (`datasize`) of 32 MiB and above, and the
/// same bound caps a decompressed `OSMHeader`.
const MAX_BLOB_LEN: u64 = 32 * 1024 * 1024;

/// What the indexdata claims about one element kind across the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct KindClaim {
    pub min_id: i64,
    pub max_id: i64,
    pub count: u64,
}

/// The walk's answer: per kind (node, way, relation) the claimed ID range
/// over blobs claiming a nonzero count, and the header's required features.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct IndexSummary {
    pub kinds: [Option<KindClaim>; 3],
    pub required_features: Vec<String>,
}

impl IndexSummary {
    pub fn count(&self, kind: usize) -> u64 {
        self.kinds
            .get(kind)
            .copied()
            .flatten()
            .map_or(0, |claim| claim.count)
    }

    pub fn is_history(&self) -> bool {
        self.required_features
            .iter()
            .any(|f| f == "HistoricalInformation")
    }
}

/// Walk `path`'s framing; refuse anything pbfhogg would not read through its
/// indexdata. Errors name the blob ordinal and offset.
pub(super) fn walk(path: &Path) -> Result<IndexSummary, DevError> {
    let mut file = std::fs::File::open(path)
        .map_err(|e| DevError::Verify(format!("cannot open {}: {e}", path.display())))?;
    let len = file
        .metadata()
        .map_err(|e| DevError::Verify(format!("cannot stat {}: {e}", path.display())))?
        .len();
    walk_reader(&mut file, len).map_err(|why| {
        DevError::Verify(format!(
            "{} is not a fully indexed PBF this check can select IDs from: {why}",
            path.display()
        ))
    })
}

fn walk_reader(r: &mut (impl Read + Seek), len: u64) -> Result<IndexSummary, String> {
    let mut summary = IndexSummary::default();
    let features = walk_frames(r, len, |indexdata| {
        let index = match indexdata {
            Indexdata::Absent => return Err("data blob has no indexdata".into()),
            Indexdata::Conflicting => return Err("data blob has two indexdata fields".into()),
            Indexdata::Present(bytes) => decode_index(&bytes)?,
        };
        fold(&mut summary, index)
    })?;
    summary.required_features = features;
    Ok(summary)
}

/// How a PBF's data blobs carry indexdata, read by the same header-only walk:
/// what pbfhogg's `check --ids` reads its kind claims from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Framing {
    /// No data blob carries indexdata.
    Unindexed,
    /// Every data blob carries indexdata, or there is no data blob (pbfhogg
    /// reads a header-only file as indexed). `claims_in_kind_order` is
    /// whether the kind every data blob claims, an empty one's included,
    /// never decreases from node to way to relation: on an indexed file
    /// pbfhogg's `check --ids` judges type order from those claims, not from
    /// the decoded elements.
    Indexed { claims_in_kind_order: bool },
}

/// Read `path`'s [`Framing`], refusing what pbfhogg could not read through
/// its indexdata (as [`walk`] does) and a file mixing data blobs with and
/// without indexdata, which pbfhogg classifies by its first blob alone.
pub(super) fn framing(path: &Path) -> Result<Framing, DevError> {
    let mut file = std::fs::File::open(path)
        .map_err(|e| DevError::Verify(format!("cannot open {}: {e}", path.display())))?;
    let len = file
        .metadata()
        .map_err(|e| DevError::Verify(format!("cannot stat {}: {e}", path.display())))?
        .len();
    framing_reader(&mut file, len).map_err(|why| {
        DevError::Verify(format!(
            "cannot read the framing of {}: {why}",
            path.display()
        ))
    })
}

fn framing_reader(r: &mut (impl Read + Seek), len: u64) -> Result<Framing, String> {
    let (mut with, mut without) = (0u64, 0u64);
    let mut highest: Option<usize> = None;
    let mut in_order = true;
    walk_frames(r, len, |indexdata| {
        match indexdata {
            Indexdata::Absent => without += 1,
            Indexdata::Conflicting => return Err("data blob has two indexdata fields".into()),
            Indexdata::Present(bytes) => {
                let index = decode_index(&bytes)?;
                with += 1;
                if highest.is_some_and(|h| index.kind < h) {
                    in_order = false;
                }
                highest = Some(highest.map_or(index.kind, |h| h.max(index.kind)));
            }
        }
        Ok(())
    })?;
    match (with, without) {
        (_, 0) => Ok(Framing::Indexed {
            claims_in_kind_order: in_order,
        }),
        (0, _) => Ok(Framing::Unindexed),
        (with, without) => Err(format!(
            "{with} data blobs carry indexdata and {without} do not"
        )),
    }
}

/// Walk every frame, handing each data blob's indexdata to `on_data` and
/// seeking past its payload; returns the header's required features.
fn walk_frames(
    r: &mut (impl Read + Seek),
    len: u64,
    mut on_data: impl FnMut(Indexdata) -> Result<(), String>,
) -> Result<Vec<String>, String> {
    let mut required_features = Vec::new();
    let mut offset: u64 = 0;
    let mut ordinal: u64 = 0;
    let mut seen_header = false;
    while offset < len {
        let at = |why: String| format!("blob {ordinal} at byte {offset}: {why}");
        let header_len_end = offset
            .checked_add(4)
            .filter(|end| *end <= len)
            .ok_or_else(|| at("truncated blob header length".into()))?;
        let mut len_bytes = [0u8; 4];
        r.read_exact(&mut len_bytes)
            .map_err(|e| at(e.to_string()))?;
        let header_len = u32::from_be_bytes(len_bytes);
        if header_len == 0 || header_len >= MAX_HEADER_LEN {
            return Err(at(format!("blob header length {header_len} out of range")));
        }
        let header_end = header_len_end
            .checked_add(u64::from(header_len))
            .filter(|end| *end <= len)
            .ok_or_else(|| at("truncated blob header".into()))?;
        let mut header = vec![0u8; header_len as usize];
        r.read_exact(&mut header).map_err(|e| at(e.to_string()))?;
        let header = parse_blob_header(&header).map_err(at)?;
        if header.datasize >= MAX_BLOB_LEN {
            return Err(at(format!("datasize {} out of range", header.datasize)));
        }
        let payload_end = header_end
            .checked_add(header.datasize)
            .filter(|end| *end <= len)
            .ok_or_else(|| at("truncated blob payload".into()))?;
        match header.blob_type.as_str() {
            "OSMHeader" if !seen_header && ordinal == 0 => {
                seen_header = true;
                let size = usize::try_from(header.datasize).map_err(|e| at(e.to_string()))?;
                let mut payload = vec![0u8; size];
                r.read_exact(&mut payload).map_err(|e| at(e.to_string()))?;
                let block = blob_content(&payload).map_err(at)?;
                required_features = header_features(&block).map_err(at)?;
            }
            "OSMHeader" => return Err(at("OSMHeader not first, or repeated".into())),
            "OSMData" if seen_header => {
                on_data(header.indexdata).map_err(at)?;
                r.seek(SeekFrom::Start(payload_end))
                    .map_err(|e| at(e.to_string()))?;
            }
            "OSMData" => return Err(at("data blob before the OSMHeader".into())),
            other => return Err(at(format!("unknown blob type {other:?}"))),
        }
        offset = payload_end;
        ordinal += 1;
    }
    if !seen_header {
        return Err("no OSMHeader blob".into());
    }
    Ok(required_features)
}

/// Append to the PBF at `path` one `OSMData` blob holding an empty
/// `PrimitiveBlock` (an empty string table, no groups) whose indexdata
/// (version 1) claims `kind` with no elements: a fixture whose decoded
/// elements can be in order while its kind claims are not. Written raw,
/// uncompressed.
pub(super) fn append_empty_claim(path: &Path, kind: usize) -> Result<(), DevError> {
    use std::io::Write as _;
    let kind = u8::try_from(kind)
        .ok()
        .filter(|k| *k <= 2)
        .ok_or_else(|| DevError::Internal(format!("element kind {kind} unknown")))?;
    let mut block = Vec::new();
    bytes_field(1, &[], &mut block);
    let mut blob = Vec::new();
    bytes_field(1, &block, &mut blob);
    varint(2 << 3, &mut blob);
    varint(block.len() as u64, &mut blob);
    let mut index = vec![1, kind];
    index.extend_from_slice(&0i64.to_le_bytes());
    index.extend_from_slice(&0i64.to_le_bytes());
    index.extend_from_slice(&0u64.to_le_bytes());
    let mut header = Vec::new();
    bytes_field(1, b"OSMData", &mut header);
    bytes_field(2, &index, &mut header);
    varint(3 << 3, &mut header);
    varint(blob.len() as u64, &mut header);
    let header_len = u32::try_from(header.len())
        .map_err(|e| DevError::Internal(format!("blob header length: {e}")))?;
    let mut frame = header_len.to_be_bytes().to_vec();
    frame.extend_from_slice(&header);
    frame.extend_from_slice(&blob);
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|e| DevError::Verify(format!("cannot open {}: {e}", path.display())))?;
    file.write_all(&frame)?;
    file.sync_all()?;
    Ok(())
}

/// A protobuf varint.
fn varint(mut v: u64, out: &mut Vec<u8>) {
    loop {
        #[allow(clippy::cast_possible_truncation)] // masked to seven bits
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// A length-delimited protobuf field.
fn bytes_field(number: u64, v: &[u8], out: &mut Vec<u8>) {
    varint(number << 3 | 2, out);
    varint(v.len() as u64, out);
    out.extend_from_slice(v);
}

fn fold(summary: &mut IndexSummary, index: BlobIndex) -> Result<(), String> {
    if index.count == 0 {
        return Ok(());
    }
    let slot = &mut summary.kinds[index.kind];
    *slot = Some(match *slot {
        None => KindClaim {
            min_id: index.min_id,
            max_id: index.max_id,
            count: index.count,
        },
        Some(claim) => KindClaim {
            min_id: claim.min_id.min(index.min_id),
            max_id: claim.max_id.max(index.max_id),
            count: claim
                .count
                .checked_add(index.count)
                .ok_or("element count overflows")?,
        },
    });
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BlobIndex {
    kind: usize,
    min_id: i64,
    max_id: i64,
    count: u64,
}

fn decode_index(bytes: &[u8]) -> Result<BlobIndex, String> {
    let version = bytes.first().copied().ok_or("empty indexdata")?;
    let declared = match version {
        1 => 26,
        2 | 3 => 42,
        other => return Err(format!("indexdata version {other} is not one brokkr reads")),
    };
    if bytes.len() != declared {
        return Err(format!(
            "indexdata version {version} is {} bytes, not {declared}",
            bytes.len()
        ));
    }
    let word = |at: usize| -> [u8; 8] {
        let mut w = [0u8; 8];
        w.copy_from_slice(&bytes[at..at + 8]);
        w
    };
    let kind = usize::from(bytes[1]);
    if kind > 2 {
        return Err(format!("indexdata kind {kind} unknown"));
    }
    let min_id = i64::from_le_bytes(word(2));
    let max_id = i64::from_le_bytes(word(10));
    let count = u64::from_le_bytes(word(18));
    if min_id > max_id {
        return Err(format!("indexdata range {min_id}..{max_id} inverted"));
    }
    Ok(BlobIndex {
        kind,
        min_id,
        max_id,
        count,
    })
}

#[derive(Debug, PartialEq, Eq)]
enum Indexdata {
    Absent,
    Present(Vec<u8>),
    Conflicting,
}

struct BlobHeader {
    blob_type: String,
    indexdata: Indexdata,
    datasize: u64,
}

fn parse_blob_header(bytes: &[u8]) -> Result<BlobHeader, String> {
    let mut blob_type = None;
    let mut indexdata = Indexdata::Absent;
    let mut datasize = None;
    for field in Fields::new(bytes) {
        match field? {
            (1, Value::Bytes(v)) => blob_type = Some(String::from_utf8_lossy(v).into_owned()),
            (2, Value::Bytes(v)) => {
                // pbfhogg ignores an empty occurrence and reads two non-empty
                // ones as conflicting claims: no usable index.
                if !v.is_empty() {
                    indexdata = match indexdata {
                        Indexdata::Absent => Indexdata::Present(v.to_vec()),
                        _ => Indexdata::Conflicting,
                    };
                }
            }
            (3, Value::Varint(v)) => datasize = Some(int32(v)?),
            (1..=3, _) => return Err("blob header field has the wrong wire type".into()),
            _ => {}
        }
    }
    let datasize = datasize.ok_or("blob header has no datasize")?;
    let datasize = u64::try_from(datasize)
        .ok()
        .filter(|d| *d > 0)
        .ok_or_else(|| format!("datasize {datasize} out of range"))?;
    Ok(BlobHeader {
        blob_type: blob_type.ok_or("blob header has no type")?,
        indexdata,
        datasize,
    })
}

/// A protobuf `int32` varint, validated per occurrence as pbfhogg does: a
/// non-negative value fits 31 bits, a negative one arrives sign-extended to
/// 64; anything else is not an `int32`.
fn int32(v: u64) -> Result<i64, String> {
    #[allow(clippy::cast_possible_wrap)] // the sign-extended reading is the point
    let signed = v as i64;
    if i32::try_from(signed).is_ok() {
        Ok(signed)
    } else {
        Err(format!("varint {v} is not an int32"))
    }
}

/// The decompressed content of a `Blob`: `raw` (field 1) or `zlib_data`
/// (field 3) with its `raw_size` (field 2). Two payload fields, or any other
/// compression, are refused.
fn blob_content(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut payload: Option<(u32, &[u8])> = None;
    let mut raw_size = None;
    for field in Fields::new(bytes) {
        match field? {
            (2, Value::Varint(v)) => {
                raw_size =
                    Some(u64::try_from(int32(v)?).map_err(|_| "raw_size is negative".to_owned())?);
            }
            (field @ (1 | 3..=7), Value::Bytes(v)) => {
                if payload.replace((field, v)).is_some() {
                    return Err("blob has more than one payload field".into());
                }
            }
            (1..=7, _) => return Err("blob field has the wrong wire type".into()),
            _ => {}
        }
    }
    match payload {
        Some((1, raw)) => {
            if raw.len() as u64 >= MAX_BLOB_LEN {
                return Err("raw header block too large".into());
            }
            // raw_size is optional on a raw blob, but a declared one must
            // match, as pbfhogg checks.
            if let Some(size) = raw_size
                && size != raw.len() as u64
            {
                return Err(format!(
                    "raw header block is {} bytes, raw_size says {size}",
                    raw.len()
                ));
            }
            Ok(raw.to_vec())
        }
        Some((3, zlib)) => {
            let size = raw_size.ok_or("zlib blob has no raw_size")?;
            if size >= MAX_BLOB_LEN {
                return Err(format!("raw_size {size} out of range"));
            }
            let size = usize::try_from(size).map_err(|e| e.to_string())?;
            let mut out = Vec::with_capacity(size);
            let mut decoder = flate2::read::ZlibDecoder::new(zlib).take(size as u64 + 1);
            decoder
                .read_to_end(&mut out)
                .map_err(|e| format!("header block does not inflate: {e}"))?;
            if out.len() != size {
                return Err(format!(
                    "header block inflates to {} bytes, raw_size says {size}",
                    out.len()
                ));
            }
            Ok(out)
        }
        Some((field, _)) => Err(format!("header blob compression field {field} unsupported")),
        None => Err("header blob has no payload".into()),
    }
}

/// `HeaderBlock.required_features` (field 4, repeated string).
fn header_features(block: &[u8]) -> Result<Vec<String>, String> {
    let mut features = Vec::new();
    for field in Fields::new(block) {
        match field? {
            (4, Value::Bytes(v)) => features.push(String::from_utf8_lossy(v).into_owned()),
            (4, _) => return Err("required_features has the wrong wire type".into()),
            _ => {}
        }
    }
    Ok(features)
}

/// Protobuf's largest field number, `2^29 - 1`.
const MAX_FIELD_NUMBER: u32 = (1 << 29) - 1;

enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed,
}

/// Protobuf fields of one message, in order. Groups (wire types 3 and 4) and
/// unknown wire types are an error.
struct Fields<'a> {
    rest: &'a [u8],
    failed: bool,
}

impl<'a> Fields<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            rest: bytes,
            failed: false,
        }
    }

    fn varint(&mut self) -> Result<u64, String> {
        let mut value: u64 = 0;
        for i in 0..10 {
            let (&b, rest) = self.rest.split_first().ok_or("truncated varint")?;
            self.rest = rest;
            // The tenth byte holds bit 63 only; anything more overflows.
            if i == 9 && b > 1 {
                return Err("varint overflows 64 bits".into());
            }
            value |= u64::from(b & 0x7f) << (7 * i);
            if b & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err("varint too long".into())
    }

    fn take(&mut self, n: u64) -> Result<&'a [u8], String> {
        let n = usize::try_from(n).map_err(|e| e.to_string())?;
        if n > self.rest.len() {
            return Err("truncated field".into());
        }
        let (head, rest) = self.rest.split_at(n);
        self.rest = rest;
        Ok(head)
    }

    fn field(&mut self) -> Result<(u32, Value<'a>), String> {
        let key = self.varint()?;
        let number = u32::try_from(key >> 3)
            .ok()
            .filter(|n| (1..=MAX_FIELD_NUMBER).contains(n))
            .ok_or("field number out of range")?;
        let value = match key & 7 {
            0 => Value::Varint(self.varint()?),
            1 => {
                self.take(8)?;
                Value::Fixed
            }
            2 => {
                let n = self.varint()?;
                Value::Bytes(self.take(n)?)
            }
            5 => {
                self.take(4)?;
                Value::Fixed
            }
            wire => return Err(format!("unsupported wire type {wire}")),
        };
        Ok((number, value))
    }
}

impl<'a> Iterator for Fields<'a> {
    type Item = Result<(u32, Value<'a>), String>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.rest.is_empty() {
            return None;
        }
        let field = self.field();
        self.failed = field.is_err();
        Some(field)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::cast_possible_truncation)]
    use super::{
        Framing, IndexSummary, KindClaim, bytes_field, framing_reader, varint, walk_reader,
    };
    use std::io::{Cursor, Write};

    fn index(version: u8, kind: u8, min: i64, max: i64, count: u64) -> Vec<u8> {
        let mut b = vec![version, kind];
        b.extend_from_slice(&min.to_le_bytes());
        b.extend_from_slice(&max.to_le_bytes());
        b.extend_from_slice(&count.to_le_bytes());
        if version != 1 {
            b.extend_from_slice(&[0u8; 16]);
        }
        b
    }

    fn frame(blob_type: &str, indexdata: &[&[u8]], payload: &[u8], out: &mut Vec<u8>) {
        let mut header = Vec::new();
        bytes_field(1, blob_type.as_bytes(), &mut header);
        for idx in indexdata {
            bytes_field(2, idx, &mut header);
        }
        varint(3 << 3, &mut header);
        varint(payload.len() as u64, &mut header);
        out.extend_from_slice(&(header.len() as u32).to_be_bytes());
        out.extend_from_slice(&header);
        out.extend_from_slice(payload);
    }

    fn header_payload(features: &[&str], zlib: bool) -> Vec<u8> {
        let mut block = Vec::new();
        for f in features {
            bytes_field(4, f.as_bytes(), &mut block);
        }
        let mut blob = Vec::new();
        if zlib {
            let mut enc =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            enc.write_all(&block).unwrap();
            let compressed = enc.finish().unwrap();
            varint(2 << 3, &mut blob);
            varint(block.len() as u64, &mut blob);
            bytes_field(3, &compressed, &mut blob);
        } else {
            bytes_field(1, &block, &mut blob);
        }
        blob
    }

    fn file(data_blobs: &[Vec<Vec<u8>>], features: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        frame("OSMHeader", &[], &header_payload(features, true), &mut out);
        for idx in data_blobs {
            let refs: Vec<&[u8]> = idx.iter().map(Vec::as_slice).collect();
            frame("OSMData", &refs, b"payload-not-read", &mut out);
        }
        out
    }

    fn run(bytes: &[u8]) -> Result<IndexSummary, String> {
        walk_reader(&mut Cursor::new(bytes), bytes.len() as u64)
    }

    #[test]
    fn folds_endpoints_per_kind_and_skips_empty_claims() {
        let bytes = file(
            &[
                vec![index(3, 0, 5, 90, 10)],
                vec![index(1, 0, 100, 200, 4)],
                vec![index(3, 0, -7, 1_000_000, 0)],
                vec![index(2, 2, 3, 3, 1)],
            ],
            &["OsmSchema-V0.6", "DenseNodes"],
        );
        let summary = run(&bytes).unwrap();
        assert_eq!(
            summary.kinds[0],
            Some(KindClaim {
                min_id: 5,
                max_id: 200,
                count: 14
            })
        );
        assert_eq!(summary.kinds[1], None);
        assert_eq!(summary.count(2), 1);
        assert!(!summary.is_history());
    }

    #[test]
    fn reads_history_from_a_raw_or_zlib_header() {
        let mut bytes = Vec::new();
        frame(
            "OSMHeader",
            &[],
            &header_payload(&["HistoricalInformation"], false),
            &mut bytes,
        );
        assert!(run(&bytes).unwrap().is_history());
        let bytes = file(&[], &["HistoricalInformation"]);
        assert!(run(&bytes).unwrap().is_history());
    }

    #[test]
    fn refuses_what_pbfhogg_cannot_read_through_indexdata() {
        let good = index(3, 0, 1, 2, 2);
        // Each case is the indexdata fields of one data blob.
        let cases: Vec<Vec<Vec<u8>>> = vec![
            vec![],
            vec![index(4, 0, 1, 2, 2)],
            vec![index(1, 0, 1, 2, 2)[..20].to_vec()],
            vec![{
                let mut v = index(1, 0, 1, 2, 2);
                v.extend_from_slice(&[0; 16]);
                v
            }],
            vec![index(3, 3, 1, 2, 2)],
            vec![index(3, 1, 9, 2, 2)],
            vec![good.clone(), good.clone()],
        ];
        for (i, fields) in cases.into_iter().enumerate() {
            assert!(run(&file(&[fields], &[])).is_err(), "case {i} accepted");
        }
        // An empty duplicate occurrence is ignored, as pbfhogg does.
        assert!(run(&file(&[vec![good, Vec::new()]], &[])).is_ok());
    }

    #[test]
    fn refuses_overflowing_varints_and_bad_field_numbers() {
        let mut overflow =
            super::Fields::new(&[0x81, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x02]);
        assert!(overflow.varint().is_err());
        let mut top =
            super::Fields::new(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01]);
        assert_eq!(top.varint().unwrap(), 1 << 63);
        assert!(super::int32(1 << 32 | 1).is_err());
        assert_eq!(super::int32(u64::MAX).unwrap(), -1);
        assert_eq!(super::int32(7).unwrap(), 7);
        // Field number zero (key 0x02: field 0, wire type 2).
        assert!(super::Fields::new(&[0x02, 0x00]).next().unwrap().is_err());
    }

    #[test]
    fn refuses_bad_framing() {
        let bytes = file(&[vec![index(3, 0, 1, 2, 2)]], &[]);
        assert!(run(&bytes[..bytes.len() - 1]).is_err(), "truncated payload");
        assert!(run(&[]).is_err(), "no header");
        let mut twice = bytes.clone();
        frame("OSMHeader", &[], &header_payload(&[], true), &mut twice);
        assert!(run(&twice).is_err(), "second header");
        let mut data_first = Vec::new();
        frame("OSMData", &[&index(3, 0, 1, 2, 2)], b"x", &mut data_first);
        assert!(run(&data_first).is_err(), "data before header");
        let mut zero = vec![0u8; 4];
        zero.extend_from_slice(&bytes);
        assert!(run(&zero).is_err(), "zero header length");
    }

    fn read_framing(bytes: &[u8]) -> Result<Framing, String> {
        framing_reader(&mut Cursor::new(bytes), bytes.len() as u64)
    }

    #[test]
    fn framing_reads_claim_order_over_every_blob_an_empty_one_included() {
        let ordered = file(
            &[
                vec![index(3, 0, 1, 2, 2)],
                vec![index(3, 0, 3, 3, 1)],
                vec![index(1, 1, 5, 5, 1)],
                vec![index(3, 2, 7, 7, 1)],
            ],
            &[],
        );
        assert_eq!(
            read_framing(&ordered).unwrap(),
            Framing::Indexed {
                claims_in_kind_order: true
            }
        );
        // An empty node claim after a way claim is out of order, though no
        // element is.
        let empty_after = file(
            &[
                vec![index(3, 0, 1, 2, 2)],
                vec![index(3, 1, 5, 5, 1)],
                vec![index(1, 0, 0, 0, 0)],
            ],
            &[],
        );
        assert_eq!(
            read_framing(&empty_after).unwrap(),
            Framing::Indexed {
                claims_in_kind_order: false
            }
        );
        // A header-only file is indexed, as pbfhogg reads it.
        assert_eq!(
            read_framing(&file(&[], &[])).unwrap(),
            Framing::Indexed {
                claims_in_kind_order: true
            }
        );
        assert_eq!(
            read_framing(&file(&[vec![], vec![]], &[])).unwrap(),
            Framing::Unindexed
        );
        assert!(read_framing(&file(&[vec![index(3, 0, 1, 2, 2)], vec![]], &[])).is_err());
        let good = index(3, 0, 1, 2, 2);
        assert!(read_framing(&file(&[vec![good.clone(), good]], &[])).is_err());
    }

    #[test]
    fn an_appended_empty_claim_reads_back_as_one() {
        let dir = crate::test_scratch::scratch("pbf-frames", "empty-claim");
        let path = dir.join("f.osm.pbf");
        std::fs::write(&path, file(&[vec![index(3, 1, 5, 5, 1)]], &[])).unwrap();
        super::append_empty_claim(&path, 0).unwrap();
        assert_eq!(
            super::framing(&path).unwrap(),
            Framing::Indexed {
                claims_in_kind_order: false
            }
        );
        let summary = super::walk(&path).unwrap();
        assert_eq!(summary.count(0), 0);
        assert_eq!(summary.count(1), 1);
        assert!(super::append_empty_claim(&path, 3).is_err());
    }
}
