//! Pre-run host checks and content-digest support.

pub mod digest;

pub use digest::{
    FileDigest, PathDigest, Xxh128, cached_path_digest, compute_file_digest,
};
