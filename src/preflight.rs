//! Pre-run host checks and content-digest support.

pub mod digest;

pub use digest::{
    DigestSession, FileDigest, PathDigest, Sha256, Xxh128, cached_path_digest, compute_file_digest,
    verify_file_digest,
};
