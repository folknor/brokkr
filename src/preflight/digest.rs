//! Parsed digest values and cached file/tree hashing.

use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use xxhash_rust::xxh3::Xxh3;

use crate::error::DevError;

/// A normalized XXH128 value. The serialized form is always 32 lowercase hex
/// characters; input may use either hex case.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Xxh128([u8; 16]);

impl FromStr for Xxh128 {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_hex::<16>(value, "XXH128").map(Self)
    }
}

impl fmt::Display for Xxh128 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(&self.0, f)
    }
}

impl fmt::Debug for Xxh128 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Xxh128({self})")
    }
}

impl Serialize for Xxh128 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Xxh128 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Xxh128Visitor;

        impl<'de> Visitor<'de> for Xxh128Visitor {
            type Value = Xxh128;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a 32-character hexadecimal XXH128 digest")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                value.parse().map_err(E::custom)
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                self.visit_str(&value)
            }
        }

        deserializer.deserialize_str(Xxh128Visitor)
    }
}

/// SHA-256 is kept distinct from brokkr's XXH128 content pins.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Sha256([u8; 32]);

impl FromStr for Sha256 {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_hex::<32>(value, "SHA-256").map(Self)
    }
}

impl fmt::Display for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(&self.0, f)
    }
}

impl fmt::Debug for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sha256({self})")
    }
}

impl Sha256 {
    /// Compute SHA-256 from a stream without requiring a platform hashing
    /// executable or buffering the whole input in memory.
    pub(crate) fn from_reader(mut reader: impl Read) -> std::io::Result<Self> {
        let mut state = Sha256State::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            state.update(&buffer[..read]);
        }
        Ok(state.finish())
    }
}

/// Incremental SHA-256, for a caller that hashes a framed encoding rather
/// than one stream (the install tree fingerprint).
pub(crate) struct Sha256State {
    words: [u32; 8],
    block: [u8; 64],
    used: usize,
    total_bytes: u64,
}

impl Sha256State {
    pub(crate) fn new() -> Self {
        Self {
            words: [
                0x6a09_e667,
                0xbb67_ae85,
                0x3c6e_f372,
                0xa54f_f53a,
                0x510e_527f,
                0x9b05_688c,
                0x1f83_d9ab,
                0x5be0_cd19,
            ],
            block: [0; 64],
            used: 0,
            total_bytes: 0,
        }
    }

    pub(crate) fn update(&mut self, mut bytes: &[u8]) {
        let byte_len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        self.total_bytes = self.total_bytes.wrapping_add(byte_len);

        if self.used != 0 {
            let copied = (self.block.len() - self.used).min(bytes.len());
            self.block[self.used..self.used + copied].copy_from_slice(&bytes[..copied]);
            self.used += copied;
            bytes = &bytes[copied..];
            if self.used == self.block.len() {
                let block = self.block;
                self.compress(&block);
                self.used = 0;
            }
        }

        while bytes.len() >= self.block.len() {
            let mut block = [0_u8; 64];
            block.copy_from_slice(&bytes[..64]);
            self.compress(&block);
            bytes = &bytes[64..];
        }

        if !bytes.is_empty() {
            self.block[..bytes.len()].copy_from_slice(bytes);
            self.used = bytes.len();
        }
    }

    pub(crate) fn finish(mut self) -> Sha256 {
        let bit_length = self.total_bytes.wrapping_mul(8);
        self.block[self.used] = 0x80;
        self.used += 1;

        if self.used > 56 {
            self.block[self.used..].fill(0);
            let block = self.block;
            self.compress(&block);
            self.block = [0; 64];
            self.used = 0;
        }

        self.block[self.used..56].fill(0);
        self.block[56..].copy_from_slice(&bit_length.to_be_bytes());
        let block = self.block;
        self.compress(&block);

        let mut digest = [0_u8; 32];
        let (digest_words, _) = digest.as_chunks_mut::<4>();
        for (chunk, word) in digest_words.iter_mut().zip(self.words) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        Sha256(digest)
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut schedule = [0_u32; 64];
        let (block_words, _) = block.as_chunks::<4>();
        for (slot, bytes) in schedule.iter_mut().zip(block_words) {
            *slot = u32::from_be_bytes(*bytes);
        }
        for index in 16..64 {
            let x = schedule[index - 15];
            let y = schedule[index - 2];
            let small_sigma_0 = x.rotate_right(7) ^ x.rotate_right(18) ^ (x >> 3);
            let small_sigma_1 = y.rotate_right(17) ^ y.rotate_right(19) ^ (y >> 10);
            schedule[index] = schedule[index - 16]
                .wrapping_add(small_sigma_0)
                .wrapping_add(schedule[index - 7])
                .wrapping_add(small_sigma_1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.words;
        for (index, word) in schedule.iter().enumerate() {
            let large_sigma_1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ (!e & g);
            let t1 = h
                .wrapping_add(large_sigma_1)
                .wrapping_add(choose)
                .wrapping_add(SHA256_K[index])
                .wrapping_add(*word);
            let large_sigma_0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let t2 = large_sigma_0.wrapping_add(majority);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }

        for (state, word) in self.words.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *state = (*state).wrapping_add(word);
        }
    }
}

const SHA256_K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

fn parse_hex<const N: usize>(value: &str, algorithm: &str) -> Result<[u8; N], String> {
    if value.len() != N * 2 {
        if algorithm == "XXH128" && value.len() == 64 {
            return Err("expected XXH128 digest (32 hexadecimal characters); got 64, which is SHA-256. brokkr expects the XXH128 algorithm.".to_owned());
        }
        return Err(format!(
            "{algorithm} digest must be exactly {} hexadecimal characters; got {}",
            N * 2,
            value.len()
        ));
    }

    let mut bytes = [0; N];
    let (pairs, _) = value.as_bytes().as_chunks::<2>();
    for (index, pair) in pairs.iter().enumerate() {
        let pair = std::str::from_utf8(pair)
            .map_err(|_| format!("{algorithm} digest must contain only hexadecimal characters"))?;
        bytes[index] = u8::from_str_radix(pair, 16)
            .map_err(|_| format!("{algorithm} digest must contain only hexadecimal characters"))?;
    }
    Ok(bytes)
}

fn write_hex(bytes: &[u8], f: &mut fmt::Formatter<'_>) -> fmt::Result {
    for byte in bytes {
        write!(f, "{byte:02x}")?;
    }
    Ok(())
}

/// A digest of one file. The wrapper prevents a tree fold from being used as
/// the pin for a file entry by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileDigest {
    value: Xxh128,
}

impl FileDigest {
    fn new(value: Xxh128) -> Self {
        Self { value }
    }

    pub fn value(&self) -> Xxh128 {
        self.value
    }
}

impl FromStr for FileDigest {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value.parse().map(Self::new)
    }
}

impl fmt::Display for FileDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.value, f)
    }
}

impl Serialize for FileDigest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.value.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for FileDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Xxh128::deserialize(deserializer).map(Self::new)
    }
}

/// A digest of a directory tree, distinct from a single file's digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TreeDigest(Xxh128);

impl TreeDigest {
    pub fn value(self) -> Xxh128 {
        self.0
    }
}

impl fmt::Display for TreeDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// The shape discovered when hashing a configured path at runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathDigest {
    File(FileDigest),
    Tree(TreeDigest),
}

impl PathDigest {
    pub fn value(&self) -> Xxh128 {
        match self {
            Self::File(digest) => digest.value(),
            Self::Tree(digest) => digest.value(),
        }
    }
}

impl fmt::Display for PathDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.value(), f)
    }
}

// XXH128 file verification with mtime cache
// ---------------------------------------------------------------------------

/// Verify a parsed file pin against a regular file.
pub fn verify_file_digest(
    path: &Path,
    expected: &FileDigest,
    project_root: &Path,
    origin: Option<&str>,
) -> Result<(), DevError> {
    match cached_path_digest(path, project_root)? {
        PathDigest::File(actual) if actual.value() == expected.value() => Ok(()),
        actual => Err(hash_mismatch(
            path,
            &expected.to_string(),
            &actual.to_string(),
            origin,
        )),
    }
}

fn hash_mismatch(path: &Path, expected: &str, actual: &str, origin: Option<&str>) -> DevError {
    let mut msg = format!(
        "hash mismatch for {}\n  expected: {expected}\n  actual:   {actual}",
        path.display(),
    );
    if let Some(origin) = origin {
        msg.push_str(&format!("\n  origin: {origin}"));
    }
    DevError::Preflight(vec![msg])
}

/// Compute a typed digest for a file or directory, preserving which shape was
/// hashed in the return type, using the mtime cache for files.
///
/// A DIRECTORY digests as the fold of its contents - see
/// [`compute_tree_digest`]. Some inputs are delivered as a directory rather
/// than a file (a Databento delivery is two `.csv.zst` archives beside three
/// JSON descriptors, and the consuming CLI takes the directory), and a pin that
/// could only name one file inside it would either leave the real input
/// unpinned or read as verified while covering a 4 KB descriptor.
pub fn cached_path_digest(path: &Path, project_root: &Path) -> Result<PathDigest, DevError> {
    let meta = std::fs::metadata(path)?;
    if meta.is_dir() {
        // Deliberately not cached against the directory's own mtime: a
        // directory's mtime tracks its entry list, not its contents, so a file
        // rewritten in place leaves it untouched and the cache would serve a
        // digest for data that no longer exists. The per-file caches inside the
        // fold are what keep a multi-gigabyte delivery from being re-read.
        return compute_tree_digest(path, project_root).map(PathDigest::Tree);
    }
    let mut cache = HashCache::load(project_root);
    let digest = hash_file_cached(path, &meta, &mut cache)?;
    cache.save();
    Ok(PathDigest::File(FileDigest::new(digest)))
}

/// One hash cache shared across many file verifications: loaded once, saved
/// once when dropped. [`verify_file_digest`] loads and saves the cache per
/// call, which across a few thousand pins means re-parsing the whole cache a
/// few thousand times; a session pays that once. Same stamp, settle and key
/// rules - only the load/save scope differs. Dropped on an error return too,
/// so the digests computed before a failure are still kept.
pub struct DigestSession {
    cache: Option<HashCache>,
}

impl DigestSession {
    pub fn new(project_root: &Path) -> Self {
        Self {
            cache: Some(HashCache::load(project_root)),
        }
    }

    /// Verify a regular file against `expected`. A directory is refused: a
    /// pin verified here names a file, never a tree fold.
    pub fn verify_file(
        &mut self,
        path: &Path,
        expected: &FileDigest,
        origin: Option<&str>,
    ) -> Result<(), DevError> {
        let meta = std::fs::metadata(path)?;
        if !meta.is_file() {
            return Err(DevError::Preflight(vec![format!(
                "{} is not a regular file - a file pin cannot verify it",
                path.display()
            )]));
        }
        let cache = self
            .cache
            .as_mut()
            .ok_or_else(|| DevError::Internal("digest session used after save".to_owned()))?;
        let actual = hash_file_cached(path, &meta, cache)?;
        if actual == expected.value() {
            Ok(())
        } else {
            Err(hash_mismatch(
                path,
                &expected.to_string(),
                &actual.to_string(),
                origin,
            ))
        }
    }
}

impl Drop for DigestSession {
    fn drop(&mut self) {
        if let Some(cache) = self.cache.take() {
            cache.save();
        }
    }
}

/// One file's digest through `cache`: a hit when the file's stamp matches the
/// recorded one, otherwise a full read, recorded for the next caller.
fn hash_file_cached(
    path: &Path,
    meta: &std::fs::Metadata,
    cache: &mut HashCache,
) -> Result<Xxh128, DevError> {
    let key = cache_key(path);
    let stamp = FileStamp::of(meta);
    if let Some(key) = &key
        && let Some(hit) = cache.lookup(key, stamp)
    {
        return Ok(hit);
    }
    let digest = hash_file(path)?;
    if let Some(key) = key {
        cache.record(key, stamp, digest);
    }
    Ok(digest)
}

/// Compute XXH128 of a file, reading in 64 KB chunks.
pub fn compute_file_digest(path: &Path) -> Result<FileDigest, DevError> {
    hash_file(path).map(FileDigest::new)
}

fn hash_file(path: &Path) -> Result<Xxh128, DevError> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Xxh3::new();
    let mut buf = vec![0u8; 64 * 1024];

    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    Ok(Xxh128(hasher.digest128().to_be_bytes()))
}

/// Compute the XXH128 digest of a directory tree.
///
/// The digest is a fold over every file beneath `root`, sorted by path relative
/// to `root`, of `<relative path>\0<file digest>\n`. Sorting is what makes it
/// reproducible - readdir order is a filesystem detail and varies between two
/// copies of identical data. The relative path is inside the fold so that
/// renaming a file, or two files swapping contents, changes the digest: a
/// delivery is its layout as well as its bytes.
///
/// Per-file digests go through the hash cache, so re-running over an
/// unchanged multi-gigabyte delivery is a stat per file rather than a re-read.
/// The cache is loaded once and saved once for the whole walk, not once per
/// file.
///
/// Symlinks are recorded by their TARGET TEXT and never followed. Following
/// them would admit cycles and would silently pull in data from outside the
/// tree being pinned; the target string is what the delivery actually contains.
///
/// An empty tree is refused. A directory with no files in it is a wrong path
/// far more often than it is a real input, and a digest over nothing would
/// verify happily forever.
pub fn compute_tree_digest(root: &Path, project_root: &Path) -> Result<TreeDigest, DevError> {
    let mut entries: Vec<(String, Xxh128)> = Vec::new();
    let mut cache = HashCache::load(project_root);
    let walked = collect_tree_entries(root, root, &mut cache, &mut entries);
    // Saved even when the walk failed part-way: the digests it did compute
    // are correct and cost a full read each.
    cache.save();
    walked?;

    if entries.is_empty() {
        return Err(DevError::Preflight(vec![format!(
            "{} is an empty directory - nothing to digest.\n  \
             A directory with no files is a wrong path more often than it is \
             an input, and a digest over nothing would verify forever.",
            root.display()
        )]));
    }

    entries.sort();

    let mut hasher = Xxh3::new();
    for (rel, digest) in &entries {
        hasher.update(rel.as_bytes());
        hasher.update(b"\0");
        hasher.update(digest.to_string().as_bytes());
        hasher.update(b"\n");
    }
    let digest = hasher.digest128();
    Ok(TreeDigest(Xxh128(digest.to_be_bytes())))
}

/// Walk `dir`, pushing `(path relative to root, digest)` for every entry.
fn collect_tree_entries(
    root: &Path,
    dir: &Path,
    cache: &mut HashCache,
    out: &mut Vec<(String, Xxh128)>,
) -> Result<(), DevError> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        // `symlink_metadata`, not `metadata`: the distinction between a symlink
        // and what it points at is the whole reason links are not followed.
        let meta = std::fs::symlink_metadata(&path)?;

        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();

        if meta.is_dir() {
            collect_tree_entries(root, &path, cache, out)?;
        } else if meta.is_symlink() {
            let target = std::fs::read_link(&path)?;
            let mut hasher = Xxh3::new();
            hasher.update(b"symlink\0");
            hasher.update(target.as_os_str().as_encoded_bytes());
            out.push((rel, Xxh128(hasher.digest128().to_be_bytes())));
        } else {
            // A regular file (or other non-directory, non-link): its
            // `symlink_metadata` is its metadata.
            out.push((rel, hash_file_cached(&path, &meta, cache)?));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The hash cache
// ---------------------------------------------------------------------------
//
// `{project_root}/.brokkr/hash_cache`, one line per file:
// `<canonical path>\t<mtime ns>\t<ctime ns>\t<inode>\t<size>\t<xxh128>`.
//
// The cache serves pinning - a hit is a claim that the bytes are the ones the
// digest describes - so every rule below errs toward a re-read:
//
// - The key is the canonical absolute path, so a relative and an absolute
//   spelling of one file share an entry. A path with a tab or line break in it
//   (or one that is not UTF-8) is simply not cached; it could not be written
//   back unambiguously.
// - The stamp is nanosecond mtime AND ctime, inode and size. ctime cannot be
//   set from userspace, so a rewrite that restores the old mtime (`touch -d`,
//   `rsync -t`, `cp -p`) still misses.
// - An entry is recorded only once the file has settled: if its mtime or ctime
//   is within `RACY_WINDOW_NS` of the moment it was hashed, a further write in
//   the same timestamp tick could leave the stamp unchanged with different
//   bytes (git's "racy index" problem, and the reason whole-second mtimes
//   served stale digests). A fresh file is hashed every time until it settles.
// - Writers serialise on `hash_cache.lock` (flock) and re-read the file under
//   it, merging their updates into what is on disk - so concurrent brokkr
//   processes no longer overwrite each other's entries.
// - Entries whose file no longer exists are pruned on every save, so the file
//   does not grow without bound.
// - A save happens once per top-level call, not once per miss, so digesting a
//   tree of N new files is one write rather than N rewrites of a growing file.
//   A `DigestSession` widens that scope to a whole batch of verifications.
//
// Lines in any other shape (including the old four-field format) are dropped
// on read, which costs one re-hash per file after an upgrade.

/// How long after a file's last change its stamp is trusted to identify its
/// contents. Generous against coarse filesystem timestamp granularity.
const RACY_WINDOW_NS: i128 = 2_000_000_000;

/// A file's identity as the cache sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    mtime_ns: i128,
    ctime_ns: i128,
    ino: u64,
    size: u64,
}

impl FileStamp {
    fn of(meta: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        let ns = |secs: i64, nsec: i64| i128::from(secs) * 1_000_000_000 + i128::from(nsec);
        Self {
            mtime_ns: ns(meta.mtime(), meta.mtime_nsec()),
            ctime_ns: ns(meta.ctime(), meta.ctime_nsec()),
            ino: meta.ino(),
            size: meta.len(),
        }
    }

    /// True when the last change is far enough in the past that another write
    /// would necessarily move the stamp. An unreadable clock is "not settled".
    fn is_settled(&self) -> bool {
        let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) else {
            return false;
        };
        let Ok(now_ns) = i128::try_from(now.as_nanos()) else {
            return false;
        };
        now_ns - self.mtime_ns.max(self.ctime_ns) > RACY_WINDOW_NS
    }

    fn render(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}",
            self.mtime_ns, self.ctime_ns, self.ino, self.size
        )
    }
}

/// The cache key for `path`: its canonical absolute path, or `None` when it
/// cannot be stored unambiguously in the line format.
fn cache_key(path: &Path) -> Option<String> {
    let canonical = std::fs::canonicalize(path).ok()?;
    let key = canonical.to_str()?.to_owned();
    if key.contains(['\t', '\n', '\r']) {
        return None;
    }
    Some(key)
}

/// Parse one cache line; `None` for anything not in the current format.
fn parse_cache_line(line: &str) -> Option<(String, FileStamp, Xxh128)> {
    let mut parts = line.split('\t');
    let key = parts.next()?.to_owned();
    let stamp = FileStamp {
        mtime_ns: parts.next()?.parse().ok()?,
        ctime_ns: parts.next()?.parse().ok()?,
        ino: parts.next()?.parse().ok()?,
        size: parts.next()?.parse().ok()?,
    };
    let digest = parts.next()?.parse().ok()?;
    if parts.next().is_some() || key.is_empty() {
        return None;
    }
    Some((key, stamp, digest))
}

/// Read every well-formed entry from the cache file. Missing or unreadable is
/// an empty cache: the cache is an optimisation, never a source of truth.
fn read_cache_file(path: &Path) -> std::collections::BTreeMap<String, (FileStamp, Xxh128)> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(parse_cache_line)
        .map(|(key, stamp, digest)| (key, (stamp, digest)))
        .collect()
}

/// The hash cache for one top-level digest request: loaded once, consulted
/// and added to in memory, merged back to disk once by [`HashCache::save`].
struct HashCache {
    dir: PathBuf,
    entries: std::collections::BTreeMap<String, (FileStamp, Xxh128)>,
    updates: Vec<(String, FileStamp, Xxh128)>,
}

impl HashCache {
    fn load(project_root: &Path) -> Self {
        let state = crate::clean::layout::ProjectState::new(project_root);
        let dir = state.root().to_path_buf();
        let entries = read_cache_file(&state.hash_cache_file());
        Self {
            dir,
            entries,
            updates: Vec::new(),
        }
    }

    fn lookup(&self, key: &str, stamp: FileStamp) -> Option<Xxh128> {
        match self.entries.get(key) {
            Some((recorded, digest)) if *recorded == stamp => Some(*digest),
            _ => None,
        }
    }

    /// Record a freshly computed digest, if the file has settled.
    fn record(&mut self, key: String, stamp: FileStamp, digest: Xxh128) {
        if !stamp.is_settled() {
            return;
        }
        self.entries.insert(key.clone(), (stamp, digest));
        self.updates.push((key, stamp, digest));
    }

    /// Merge this request's new entries into the on-disk cache. Best-effort:
    /// a cache that cannot be written costs a re-hash next time, never a
    /// failed command.
    fn save(self) {
        if self.updates.is_empty() {
            return;
        }
        // The cache is advisory - a lost save costs a re-hash next run, never a
        // wrong digest - so a failure is reported, not propagated.
        if let Err(error) = self.try_save() {
            crate::output::warn(&format!(
                "hash cache not saved under {}: {error}",
                self.dir.display()
            ));
        }
    }

    fn try_save(&self) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        std::fs::create_dir_all(&self.dir)?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join("hash_cache.lock"))?;
        // SAFETY: `lock` is an open file descriptor owned for the duration of
        // the call; the lock is released when `lock` is dropped (closed).
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error());
        }

        // Re-read under the lock: another process may have saved since `load`.
        let cache_path = self.dir.join("hash_cache");
        let mut merged = read_cache_file(&cache_path);
        for (key, stamp, digest) in &self.updates {
            merged.insert(key.clone(), (*stamp, *digest));
        }
        merged.retain(|key, _| Path::new(key).exists());

        let mut text = String::new();
        for (key, (stamp, digest)) in &merged {
            text.push_str(&format!("{key}\t{}\t{digest}\n", stamp.render()));
        }
        // Atomic replace so a reader outside the lock never sees a partial file.
        let written = crate::atomic_write::replace(&cache_path, text.as_bytes());
        drop(lock);
        written
    }
}

#[cfg(test)]
mod sha256_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn digest(bytes: &[u8]) -> String {
        Sha256::from_reader(bytes).unwrap().to_string()
    }

    /// FIPS 180-2 vectors: the empty message, one block, the two-block
    /// padding case (56 bytes leaves no room for the length), and a message
    /// longer than the reader's buffer so block carry-over across reads runs.
    #[test]
    fn matches_the_published_vectors() {
        assert_eq!(
            digest(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            digest(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            digest(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(
            digest(&vec![b'a'; 1_000_000]),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }
}

#[cfg(test)]
mod hash_cache_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Backdate a file's mtime so it counts as settled. Its ctime stays "now",
    /// which is what makes the settle check interesting: see below.
    fn backdate(path: &Path) {
        let an_hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(an_hour_ago)
            .unwrap();
    }

    fn cache_text(root: &Path) -> String {
        std::fs::read_to_string(crate::clean::layout::ProjectState::new(root).hash_cache_file())
            .unwrap_or_default()
    }

    #[test]
    fn a_just_written_file_is_not_cached() {
        let root = crate::test_scratch::scratch("preflight", "fresh_not_cached");
        let file = root.join("f.bin");
        std::fs::write(&file, b"fresh").unwrap();
        backdate(&file);
        // mtime is an hour old but ctime is now: a same-tick rewrite could
        // still leave the stamp unchanged, so nothing may be recorded yet.
        let cached = cached_path_digest(&file, &root).unwrap();
        let PathDigest::File(cached) = cached else {
            panic!("a regular file produced a tree digest");
        };
        assert_eq!(cached, compute_file_digest(&file).unwrap());
        assert!(
            !cache_text(&root).contains("f.bin"),
            "{}",
            cache_text(&root)
        );
    }

    #[test]
    fn a_digest_session_serves_the_cache_and_refuses_mismatches_and_directories() {
        let root = crate::test_scratch::scratch("preflight", "digest_session");
        let file = root.join("s.bin");
        std::fs::write(&file, b"session").unwrap();
        let real = compute_file_digest(&file).unwrap();
        // A cache entry for the file's exact stamp with a made-up digest: the
        // session must answer from it, never re-read the bytes.
        let planted: Xxh128 = "000000000000000000000000feedface".parse().unwrap();
        let key = cache_key(&file).unwrap();
        let stamp = FileStamp::of(&std::fs::metadata(&file).unwrap());
        let mut seed = HashCache::load(&root);
        seed.updates.push((key, stamp, planted));
        seed.save();

        let mut session = DigestSession::new(&root);
        session
            .verify_file(&file, &FileDigest::new(planted), None)
            .unwrap();
        let err = session.verify_file(&file, &real, Some("o")).unwrap_err();
        assert!(format!("{err:?}").contains("hash mismatch"), "{err:?}");
        let err = session.verify_file(&root, &real, None).unwrap_err();
        assert!(format!("{err:?}").contains("not a regular file"), "{err:?}");
    }

    #[test]
    fn a_settled_entry_is_keyed_canonically_and_served() {
        let root = crate::test_scratch::scratch("preflight", "settled_served");
        let file = root.join("g.bin");
        std::fs::write(&file, b"settled").unwrap();
        let meta = std::fs::metadata(&file).unwrap();
        let mut stamp = FileStamp::of(&meta);
        // Pretend the change happened long ago so the entry is recordable.
        stamp.mtime_ns -= 10 * RACY_WINDOW_NS;
        stamp.ctime_ns -= 10 * RACY_WINDOW_NS;
        let mut cache = HashCache::load(&root);
        let digest: Xxh128 = "000000000000000000000000feedface".parse().unwrap();
        cache.record(cache_key(&file).unwrap(), stamp, digest);
        cache.save();

        let reloaded = HashCache::load(&root);
        // A relative-vs-absolute spelling resolves to one key.
        let key = cache_key(&root.join(".").join("g.bin")).unwrap();
        assert_eq!(reloaded.lookup(&key, stamp), Some(digest));
        // Any change to the stamp - here the real, current one - misses.
        assert_eq!(reloaded.lookup(&key, FileStamp::of(&meta)), None);
    }

    #[test]
    fn entries_for_deleted_files_are_pruned_on_save() {
        let root = crate::test_scratch::scratch("preflight", "pruned");
        let gone = root.join("gone.bin");
        let kept = root.join("kept.bin");
        std::fs::write(&gone, b"a").unwrap();
        std::fs::write(&kept, b"b").unwrap();
        let old = |p: &Path| {
            let mut s = FileStamp::of(&std::fs::metadata(p).unwrap());
            s.mtime_ns -= 10 * RACY_WINDOW_NS;
            s.ctime_ns -= 10 * RACY_WINDOW_NS;
            s
        };
        let mut cache = HashCache::load(&root);
        cache.record(
            cache_key(&gone).unwrap(),
            old(&gone),
            "000000000000000000000000000000aa".parse().unwrap(),
        );
        cache.save();
        std::fs::remove_file(&gone).unwrap();

        let mut cache = HashCache::load(&root);
        cache.record(
            cache_key(&kept).unwrap(),
            old(&kept),
            "000000000000000000000000000000bb".parse().unwrap(),
        );
        cache.save();
        let text = cache_text(&root);
        assert!(
            text.contains("kept.bin") && !text.contains("gone.bin"),
            "{text}"
        );
    }

    #[test]
    fn malformed_and_old_format_lines_are_ignored() {
        assert!(parse_cache_line("/a\t1\t2\tdeadbeef").is_none());
        assert!(parse_cache_line("/a\tx\t1\t2\t3\t00000000000000000000000000000000").is_none());
        assert!(
            parse_cache_line("/a\t1\t2\t3\t4\t00000000000000000000000000000000\textra").is_none()
        );
        let (key, stamp, digest) =
            parse_cache_line("/a\t1\t2\t3\t4\t000000000000000000000000deadbeef").unwrap();
        assert_eq!(key.as_str(), "/a");
        assert_eq!(digest.to_string(), "000000000000000000000000deadbeef");
        assert_eq!(
            stamp,
            FileStamp {
                mtime_ns: 1,
                ctime_ns: 2,
                ino: 3,
                size: 4
            }
        );
    }
}

#[cfg(test)]
mod tree_hash_tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use std::path::PathBuf;

    /// Through the shared allocator, whose fixed per-test path is cleared on
    /// reuse - a pid-and-time-stamped name never repeats, so it is never
    /// cleared, and every run used to leave six more dirs in `target/`.
    fn tmpdir(name: &str) -> PathBuf {
        crate::test_scratch::scratch("preflight-tree", name)
    }

    /// Build a delivery-shaped tree: two archives beside their descriptors.
    fn delivery(root: &Path) {
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(root.join("mnq-0.csv.zst"), b"first archive").unwrap();
        std::fs::write(root.join("mnq-1.csv.zst"), b"second archive").unwrap();
        std::fs::write(root.join("manifest.json"), b"{\"files\":[]}").unwrap();
        std::fs::write(root.join("metadata.json"), b"{}").unwrap();
    }

    #[test]
    fn identical_trees_digest_identically() {
        let base = tmpdir("identical");
        let a = base.join("a");
        let b = base.join("b");
        delivery(&a);
        delivery(&b);
        // Two copies of the same delivery must agree despite living at
        // different paths and being read in whatever order readdir gives -
        // which is a filesystem detail, and is why the fold sorts.
        assert_eq!(
            compute_tree_digest(&a, &base).unwrap(),
            compute_tree_digest(&b, &base).unwrap()
        );
    }

    #[test]
    fn a_changed_file_changes_the_tree_digest() {
        let base = tmpdir("changed");
        let root = base.join("d");
        delivery(&root);
        let before = compute_tree_digest(&root, &base).unwrap();
        std::fs::write(root.join("mnq-0.csv.zst"), b"different archive").unwrap();
        assert_ne!(before, compute_tree_digest(&root, &base).unwrap());
    }

    #[test]
    fn a_renamed_file_changes_the_tree_digest() {
        let base = tmpdir("renamed");
        let root = base.join("d");
        delivery(&root);
        let before = compute_tree_digest(&root, &base).unwrap();
        std::fs::rename(root.join("mnq-0.csv.zst"), root.join("mnq-2.csv.zst")).unwrap();
        // Same bytes, different layout. A delivery is its layout too - the
        // consuming CLI resolves files inside it by name.
        assert_ne!(before, compute_tree_digest(&root, &base).unwrap());
    }

    #[test]
    fn nested_directories_are_included() {
        let base = tmpdir("nested");
        let root = base.join("d");
        delivery(&root);
        let before = compute_tree_digest(&root, &base).unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/extra.json"), b"{}").unwrap();
        assert_ne!(before, compute_tree_digest(&root, &base).unwrap());
    }

    #[test]
    fn an_empty_tree_is_refused() {
        let base = tmpdir("empty");
        let root = base.join("d");
        std::fs::create_dir_all(&root).unwrap();
        let err = compute_tree_digest(&root, &base).unwrap_err();
        let DevError::Preflight(msgs) = err else {
            panic!("expected a preflight refusal, got {err:?}");
        };
        assert!(msgs.join(" ").contains("empty directory"), "{msgs:?}");
    }

    #[test]
    fn cached_path_digest_dispatches_on_file_versus_directory() {
        let base = tmpdir("dispatch");
        let root = base.join("d");
        delivery(&root);
        // The entry point the corpus registry and `brokkr env` both go through.
        let tree = cached_path_digest(&root, &base).unwrap();
        assert_eq!(
            tree,
            PathDigest::Tree(compute_tree_digest(&root, &base).unwrap())
        );
        // A directory's digest is not any one file's digest - which is exactly
        // the failure mode of pinning `manifest.json` and calling the delivery
        // pinned.
        let manifest = cached_path_digest(&root.join("manifest.json"), &base).unwrap();
        assert_ne!(tree.value(), manifest.value());
    }
}
