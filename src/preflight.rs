//! Pre-run host checks and content-digest support.

pub mod digest;
pub mod host_checks;

pub use digest::{
    FileDigest, PathDigest, Sha256, Xxh128, cached_path_digest, compute_file_digest,
    verify_file_digest,
};
pub use host_checks::{run_preflight, uring_checks};
