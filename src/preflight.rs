//! Pre-run host checks and content-digest support.

pub mod digest;
pub mod host_checks;

pub use digest::{
    FileDigest, PathDigest, Sha256, Xxh128, cached_path_digest, cached_xxh128, compute_xxh128,
    verify_file_digest, verify_file_hash,
};
pub use host_checks::{run_preflight, uring_checks};
