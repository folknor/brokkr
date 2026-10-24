//! Inputs and artifact identities carried by a prepared test plan.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

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
    /// Set only on a carrier brokkr derived for a complete claim: every
    /// rustdoc must name its package directory, which must be here. A
    /// library whose manifest sets `doctest = false` is declined - its rustdoc
    /// exits successfully without running, and the decline is recorded -
    /// since an explicit `cargo test --doc` would test it regardless; one not
    /// here at all is refused. A configured `doc_only` lane holds `None`: its
    /// author asked for `--doc` and gets cargo's meaning of it.
    pub(crate) rustdoc_admission: Option<BTreeMap<PathBuf, LibraryDoctest>>,
}

/// One member library rustdoc can test, as a derived carrier admits it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LibraryDoctest {
    pub(crate) package: String,
    /// The manifest leaves `doctest` on.
    pub(crate) doctested: bool,
}

/// A package directory as the admission table keys it: canonical where the
/// path resolves, as given otherwise - cargo's `CARGO_MANIFEST_DIR` and the
/// metadata's manifest path must meet on one spelling.
pub(crate) fn canonical_package_dir(dir: &Path) -> PathBuf {
    fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf())
}
