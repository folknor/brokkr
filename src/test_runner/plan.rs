//! Inputs and artifact identities carried by a prepared test plan.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::Path;

/// xxh3-64 of a file's contents, hex, streamed: the identity a plan records
/// for an executable, and checks again before it runs.
pub(crate) fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    let mut buf = vec![0_u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:016x}", hasher.digest()))
}

/// What a strict session admits.
#[derive(Clone, Default, Debug)]
pub(crate) struct StrictPlan {
    /// Executable path -> content hash: the harnesses this run may attribute.
    /// A harness is admitted only when its path is here AND the file at that
    /// path still hashes to the planned value at its handshake - the last
    /// moment before it runs.
    pub(crate) known: HashMap<String, String>,
    /// Whether this run is a planned doctest carrier: only then may a rustdoc
    /// stream connect.
    pub(crate) rustdoc: bool,
}
