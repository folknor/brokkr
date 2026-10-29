//! What one process of a measured scenario ran (`docs/commands/library.md`,
//! Measuring a scenario): its plan and each call's context in brokkr-json-v1
//! with their digests, the identity of every dataset file and generated role
//! it read, and the harness's features. Every iteration of a measurement
//! must agree on it, and it is what a stored row pairs on; the CLI reuse key
//! reads its input identities.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use super::describe::RoleKind;
use super::json::{self, Json};

/// A canonical document and its digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Document {
    /// `plan-v1:<32 hex>` or `context-v1:<32 hex>`.
    pub(crate) digest: String,
    /// The document in brokkr-json-v1.
    pub(crate) canonical: String,
}

impl Document {
    /// `value` canonicalized, digested under `kind` (`plan` or `context`):
    /// XXH3-128 seed 0 of the canonical bytes, lowercase hex.
    pub(crate) fn of(kind: &str, value: &Json) -> Result<Self, String> {
        let canonical = json::canonical(value)?;
        let digest = format!(
            "{kind}-v1:{:032x}",
            xxhash_rust::xxh3::xxh3_128(canonical.as_bytes())
        );
        Ok(Self { digest, canonical })
    }
}

/// One process's identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Identity {
    pub(crate) plan: Document,
    /// Each call's context, by call id.
    pub(crate) contexts: BTreeMap<String, Document>,
    /// Each dataset role's file, `xxh128:<hex>`.
    pub(crate) datasets: BTreeMap<String, String>,
    /// Each generated role after the harness exited (see [`generated`]).
    pub(crate) generated: BTreeMap<String, String>,
    /// Each seeded output role's seed recipe (`seed-v1:<kind>`); empty when
    /// the scenario seeds none.
    pub(crate) seeds: BTreeMap<String, String>,
    /// The page-cache preparation each process went through
    /// ([`super::prep::PROCEDURE`]), for a library against CLI measurement;
    /// `None` for every other.
    pub(crate) prep: Option<String>,
    /// The harness build's features, sorted, comma-joined; empty for none.
    pub(crate) features: String,
}

impl Identity {
    /// The identity as one canonical JSON text: the plan digest, each
    /// call's context digest, the dataset and generated identities, the
    /// seeds when there are any (so an identity without seeds is spelled as
    /// before seeds existed) and the features. A stored row carries it as
    /// its arm qualifier and as `meta.library.identity`.
    pub(crate) fn qualifier(&self) -> String {
        let map = |m: &BTreeMap<String, String>| {
            Json::Object(
                m.iter()
                    .map(|(k, v)| (k.clone(), Json::Str(v.clone())))
                    .collect(),
            )
        };
        let contexts: BTreeMap<String, String> = self
            .contexts
            .iter()
            .map(|(call, doc)| (call.clone(), doc.digest.clone()))
            .collect();
        let mut members = vec![
            ("v".to_owned(), Json::Int(1)),
            ("plan".to_owned(), Json::Str(self.plan.digest.clone())),
            ("contexts".to_owned(), map(&contexts)),
            ("datasets".to_owned(), map(&self.datasets)),
            ("generated".to_owned(), map(&self.generated)),
            ("features".to_owned(), Json::Str(self.features.clone())),
        ];
        if !self.seeds.is_empty() {
            members.push(("seeds".to_owned(), map(&self.seeds)));
        }
        if let Some(prep) = &self.prep {
            members.push(("prep".to_owned(), Json::Str(prep.clone())));
        }
        json::canonical(&Json::Object(members)).unwrap_or_default()
    }

    /// Every canonical document, by digest, each once.
    pub(crate) fn documents(&self) -> Vec<(String, String)> {
        let mut out: BTreeMap<String, String> = BTreeMap::new();
        out.insert(self.plan.digest.clone(), self.plan.canonical.clone());
        for doc in self.contexts.values() {
            out.insert(doc.digest.clone(), doc.canonical.clone());
        }
        out.into_iter().collect()
    }

    /// Where this identity first differs from `baseline`'s: the plan (with
    /// the first differing field), a call's context (likewise), a generated
    /// or dataset role. `None` when they agree.
    pub(crate) fn disagreement(&self, baseline: &Self) -> Option<String> {
        if self.plan.digest != baseline.plan.digest {
            let field = json::canonical_difference(&baseline.plan.canonical, &self.plan.canonical)
                .unwrap_or_default();
            return Some(format!(
                "the plan differs ({} vs {}), first at {field}",
                baseline.plan.digest, self.plan.digest
            ));
        }
        let calls: std::collections::BTreeSet<&String> = self
            .contexts
            .keys()
            .chain(baseline.contexts.keys())
            .collect();
        for call in calls {
            match (baseline.contexts.get(call), self.contexts.get(call)) {
                (Some(a), Some(b)) if a.digest != b.digest => {
                    let field =
                        json::canonical_difference(&a.canonical, &b.canonical).unwrap_or_default();
                    return Some(format!(
                        "the context of call {call} differs ({} vs {}), first at {field}",
                        a.digest, b.digest
                    ));
                }
                (Some(_), Some(_)) => {}
                _ => return Some(format!("call {call} has a context in only one")),
            }
        }
        for (what, a, b) in [
            ("generated", &baseline.generated, &self.generated),
            ("dataset", &baseline.datasets, &self.datasets),
            ("seeded", &baseline.seeds, &self.seeds),
        ] {
            let roles: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
            for role in roles {
                let (x, y) = (a.get(role), b.get(role));
                if x != y {
                    return Some(format!(
                        "{what} role {role} differs ({} vs {})",
                        x.map_or("none", String::as_str),
                        y.map_or("none", String::as_str)
                    ));
                }
            }
        }
        if self.prep != baseline.prep {
            return Some(format!(
                "the page-cache preparation differs ({:?} vs {:?})",
                baseline.prep, self.prep
            ));
        }
        (self.features != baseline.features).then(|| {
            format!(
                "the harness features differ ({:?} vs {:?})",
                baseline.features, self.features
            )
        })
    }
}

/// The identity of a dataset file: `xxh128:` and its XXH128 (through the
/// hash cache, so a file already verified is not read again).
pub(crate) fn dataset(path: &Path, project_root: &Path) -> Result<String, crate::error::DevError> {
    match crate::preflight::digest::cached_path_digest(path, project_root)? {
        crate::preflight::digest::PathDigest::File(digest) => Ok(format!("xxh128:{digest}")),
        crate::preflight::digest::PathDigest::Tree(_) => Err(crate::error::DevError::Config(
            format!("dataset file {} is a directory", path.display()),
        )),
    }
}

/// The identity of a generated role at `path` after the harness exited,
/// read without following a link: `absent`; for a file or PBF role
/// `xxh128:<hex>` of its bytes; for a directory role `tree-v1:<hex>`, the
/// XXH3-128 over its entries sorted by the raw bytes of their path relative
/// to it, each `<32 hex XXH3-128 of the path bytes><kind><value>\n`, kind `d`
/// (no value), `f` (the file's XXH128) or `l` (the XXH3-128 of the link's
/// raw target bytes), so every field has a fixed width. Any other entry, or
/// an entry not of the role's kind, cannot be identified.
pub(crate) fn generated(path: &Path, kind: RoleKind) -> Result<String, String> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok("absent".to_owned()),
        Err(e) => return Err(format!("{}: cannot inspect: {e}", path.display())),
    };
    match kind {
        RoleKind::Pbf | RoleKind::File if meta.file_type().is_file() => {
            Ok(format!("xxh128:{}", file_digest(path)?))
        }
        RoleKind::Directory if meta.file_type().is_dir() => {
            let mut entries = Vec::new();
            walk(path, path, &mut entries)?;
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut hasher = xxhash_rust::xxh3::Xxh3::new();
            for (rel, record) in &entries {
                hasher.update(format!("{:032x}", xxhash_rust::xxh3::xxh3_128(rel)).as_bytes());
                hasher.update(record.as_bytes());
                hasher.update(b"\n");
            }
            Ok(format!("tree-v1:{:032x}", hasher.digest128()))
        }
        _ => Err(format!(
            "{} is not a {}, so it cannot be identified",
            path.display(),
            kind.name()
        )),
    }
}

/// Every entry below `dir`, as (path bytes relative to `root`, kind and
/// value).
fn walk(root: &Path, dir: &Path, out: &mut Vec<(Vec<u8>, String)>) -> Result<(), String> {
    let read =
        std::fs::read_dir(dir).map_err(|e| format!("{}: cannot list: {e}", dir.display()))?;
    for entry in read {
        let entry = entry.map_err(|e| format!("{}: cannot list: {e}", dir.display()))?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|e| format!("{}: cannot inspect: {e}", path.display()))?;
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .as_os_str()
            .as_bytes()
            .to_vec();
        if kind.is_dir() {
            out.push((rel, "d".to_owned()));
            walk(root, &path, out)?;
        } else if kind.is_file() {
            out.push((rel, format!("f{}", file_digest(&path)?)));
        } else if kind.is_symlink() {
            let target = std::fs::read_link(&path)
                .map_err(|e| format!("{}: cannot read the link: {e}", path.display()))?;
            out.push((
                rel,
                format!(
                    "l{:032x}",
                    xxhash_rust::xxh3::xxh3_128(target.as_os_str().as_bytes())
                ),
            ));
        } else {
            return Err(format!(
                "{} is neither a file, a directory nor a link, so it cannot be identified",
                path.display()
            ));
        }
    }
    Ok(())
}

/// XXH128 of a regular file, opened without following a link or blocking,
/// its type checked on the open descriptor.
fn file_digest(path: &Path) -> Result<String, String> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let shown = path.display();
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| format!("{shown}: cannot open: {e}"))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("{shown}: cannot inspect: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{shown}: not a regular file"));
    }
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("{shown}: cannot read: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:032x}", hasher.digest128()))
}

#[cfg(test)]
#[path = "identity_tests.rs"]
mod tests;
