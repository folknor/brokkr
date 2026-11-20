//! Git LFS pointers left in a `--commit` worktree.
//!
//! A worktree is cut with `GIT_LFS_SKIP_DOWNLOAD_ERRORS=1`, so an LFS object
//! the server no longer has (or cannot serve right now) is left as its pointer
//! file instead of failing the checkout. Commits whose LFS history has rotted
//! stay buildable; the cost is that some tracked files may hold pointer text
//! rather than their content. [`inspect`] finds those files on every admission
//! of a worktree, cut or reused, so the caller can say so.
//!
//! Found by content, not by attribute: a candidate is a regular-file blob of at
//! most [`POINTER_MAX`] bytes in the commit's tree, and it is unresolved when
//! the file on disk parses as an LFS pointer. An attribute is a routing rule
//! that can change after the checkout (an `info/attributes` edit) or differ
//! from the one the blob was committed under; the bytes on disk are what a
//! build or a script reads.
//!
//! The scope is the snapshot at admission: tracked files of this repository,
//! not the contents of submodules, not the integrity of hydrated payloads, and
//! not what the build reads later.

use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::error::DevError;

/// Git LFS never treats a blob over this size as a pointer.
const POINTER_MAX: u64 = 1024;

/// The version URLs git-lfs accepts on a pointer's first line.
const VERSIONS: [&str; 3] = [
    "https://git-lfs.github.com/spec/v1",
    "https://hawser.github.com/spec/v1",
    "http://git-media.io/v/2",
];

/// One tracked file left as an LFS pointer.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pointer {
    /// Repository-relative path, lossily decoded for display and storage.
    pub path: String,
    /// The object the pointer names, `sha256:<hex>`.
    pub oid: String,
}

/// The tracked files of the checkout at `checkout` whose content is an LFS
/// pointer, sorted by path. A tracked file missing from the checkout is an
/// error: `--commit` worktrees are full checkouts.
pub fn inspect(checkout: &Path) -> Result<Vec<Pointer>, DevError> {
    let listing = ls_tree(checkout)?;
    let mut found = Vec::new();
    for entry in listing.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let Some((path, size)) = candidate(entry)? else {
            continue;
        };
        if size > POINTER_MAX {
            continue;
        }
        let rel = Path::new(OsStr::from_bytes(path));
        let Some(oid) = read_pointer(&checkout.join(rel), rel)? else {
            continue;
        };
        found.push(Pointer {
            path: rel.to_string_lossy().into_owned(),
            oid,
        });
    }
    found.sort();
    Ok(found)
}

/// `git ls-tree -r -l -z HEAD`, raw bytes: paths are NUL-framed and must not
/// pass through a lossy, trimming reader.
fn ls_tree(checkout: &Path) -> Result<Vec<u8>, DevError> {
    let output = crate::output::child::command_output(
        crate::process::spawn::ChildPolicy::plain_command("git")
            .args(["ls-tree", "-r", "-l", "-z", "--full-tree", "HEAD"])
            .current_dir(checkout),
    )
    .map_err(|error| DevError::Spawn {
        program: "git".into(),
        error,
    })?;
    if !output.status.success() {
        return Err(DevError::Subprocess {
            program: "git".into(),
            ending: crate::error::ProcessEnding::from_status(&output.status),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(output.stdout)
}

/// One `ls-tree -l` record, `<mode> <type> <oid> <size>\t<path>`: the path
/// and blob size of a regular file, `None` for a symlink, gitlink or tree.
fn candidate(entry: &[u8]) -> Result<Option<(&[u8], u64)>, DevError> {
    let malformed = || {
        DevError::Config(format!(
            "unreadable git ls-tree record {:?}",
            String::from_utf8_lossy(entry)
        ))
    };
    let tab = entry
        .iter()
        .position(|b| *b == b'\t')
        .ok_or_else(malformed)?;
    let (meta, path) = (&entry[..tab], &entry[tab + 1..]);
    let meta = std::str::from_utf8(meta).map_err(|_| malformed())?;
    let mut fields = meta.split_ascii_whitespace();
    let (Some(mode), Some(kind), Some(_oid), Some(size)) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return Err(malformed());
    };
    if kind != "blob" || !matches!(mode, "100644" | "100755") {
        return Ok(None);
    }
    let size = size.parse::<u64>().map_err(|_| malformed())?;
    Ok(Some((path, size)))
}

/// The oid of the pointer at `file`, `None` when its content is anything
/// else. Opened without following a symlink and read no further than a
/// pointer can reach, so a hydrated payload of any size costs one short read.
fn read_pointer(file: &Path, rel: &Path) -> Result<Option<String>, DevError> {
    let opened = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(file);
    let handle = match opened {
        Ok(handle) => handle,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(DevError::Refused(format!(
                "{} is tracked at this commit but missing from the worktree at {}; a --commit \
                 worktree must be a full checkout (is sparse checkout configured for the repo?)",
                crate::output::path(rel),
                crate::output::path(file)
            )));
        }
        // A symlink where the tree has a regular file is not a pointer.
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Ok(None),
        Err(e) => {
            return Err(DevError::Io(std::io::Error::new(
                e.kind(),
                format!(
                    "cannot read {} to look for an LFS pointer: {e}",
                    crate::output::path(file)
                ),
            )));
        }
    };
    if !handle.metadata()?.is_file() {
        return Ok(None);
    }
    let mut head = Vec::new();
    handle.take(POINTER_MAX + 1).read_to_end(&mut head)?;
    if head.len() as u64 > POINTER_MAX {
        return Ok(None);
    }
    Ok(parse(&head))
}

/// The oid of a git-lfs pointer, `None` for any other content. A pointer is
/// `key value` lines: `version` first with a known URL, a `sha256` `oid`, a
/// decimal `size`, and any `ext-*` extension lines.
fn parse(text: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(text).ok()?;
    let mut lines = text.lines();
    let version = lines.next()?.strip_prefix("version ")?;
    if !VERSIONS.contains(&version.trim_end()) {
        return None;
    }
    let (mut oid, mut size) = (None, None);
    for line in lines.filter(|l| !l.is_empty()) {
        let (key, value) = line.split_once(' ')?;
        match key {
            "oid" => {
                let hex = value.strip_prefix("sha256:")?;
                if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return None;
                }
                oid = Some(value.to_owned());
            }
            "size" => size = Some(value.parse::<u64>().ok()?),
            k if k.starts_with("ext-") => {}
            _ => return None,
        }
    }
    size?;
    oid
}

/// The operator line for a worktree admitted with pointers: how many, and the
/// first few by path. `None` when there are none.
pub fn warning(commit: &str, pointers: &[Pointer]) -> Option<String> {
    const SHOWN: usize = 10;
    if pointers.is_empty() {
        return None;
    }
    let mut paths: Vec<&str> = pointers
        .iter()
        .take(SHOWN)
        .map(|p| p.path.as_str())
        .collect();
    let rest = pointers.len().saturating_sub(SHOWN);
    let more = format!("{rest} more");
    if rest > 0 {
        paths.push(&more);
    }
    Some(format!(
        "worktree for {commit}: {} tracked file{} left as Git LFS pointer{} (object unreachable \
         when the worktree was cut), a build or script reading {} sees pointer text: {}",
        pointers.len(),
        if pointers.len() == 1 { "" } else { "s" },
        if pointers.len() == 1 { "" } else { "s" },
        if pointers.len() == 1 { "it" } else { "them" },
        paths.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::{Pointer, candidate, inspect, parse};
    use std::path::Path;

    fn git(dir: &Path, args: &[&str]) {
        let out = crate::output::child::command_output(
            crate::process::spawn::ChildPolicy::plain_command("git")
                .args(["-c", "user.email=t@example.invalid", "-c", "user.name=t"])
                .args(args)
                .current_dir(dir),
        )
        .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A committed file whose content is a pointer is found whatever the
    /// attributes say; ordinary files, large files and symlinks are not.
    #[test]
    fn inspect_finds_pointer_content_in_tracked_files() {
        let root = crate::test_scratch::scratch("worktree_lfs", "inspect");
        git(&root, &["init", "-q"]);
        let pointer = format!("version https://git-lfs.github.com/spec/v1\noid {OID}\nsize 9\n");
        std::fs::create_dir_all(root.join("data")).expect("mkdir");
        std::fs::write(root.join("data/feed.csv"), &pointer).expect("write");
        std::fs::write(root.join("plain.txt"), "hello\n").expect("write");
        std::fs::write(root.join("big.bin"), vec![b'x'; 4096]).expect("write");
        std::os::unix::fs::symlink("data/feed.csv", root.join("link")).expect("symlink");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "fixture"]);
        assert_eq!(
            inspect(&root).expect("inspect"),
            vec![Pointer {
                path: "data/feed.csv".into(),
                oid: OID.into()
            }]
        );
        std::fs::remove_file(root.join("plain.txt")).expect("rm");
        let missing = inspect(&root).expect_err("missing tracked file");
        assert!(format!("{missing}").contains("full checkout"), "{missing}");
    }

    const OID: &str = "sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393";

    #[test]
    fn a_pointer_parses_to_its_oid() {
        let text = format!("version https://git-lfs.github.com/spec/v1\noid {OID}\nsize 12345\n");
        assert_eq!(parse(text.as_bytes()).as_deref(), Some(OID));
        let legacy = format!("version https://hawser.github.com/spec/v1\noid {OID}\nsize 1\n");
        assert_eq!(parse(legacy.as_bytes()).as_deref(), Some(OID));
        let ext = format!(
            "version https://git-lfs.github.com/spec/v1\next-0-foo sha256:{}\noid {OID}\nsize 3\n",
            "0".repeat(64)
        );
        assert_eq!(parse(ext.as_bytes()).as_deref(), Some(OID));
    }

    #[test]
    fn other_content_is_not_a_pointer() {
        assert_eq!(parse(b"hello\n"), None);
        assert_eq!(parse(b"version https://git-lfs.github.com/spec/v1\n"), None);
        let no_size = format!("version https://git-lfs.github.com/spec/v1\noid {OID}\n");
        assert_eq!(parse(no_size.as_bytes()), None);
        let bad_oid = "version https://git-lfs.github.com/spec/v1\noid sha256:abc\nsize 1\n";
        assert_eq!(parse(bad_oid.as_bytes()), None);
        let unknown = format!("version https://example.com/v9\noid {OID}\nsize 1\n");
        assert_eq!(parse(unknown.as_bytes()), None);
        let extra = format!("version https://git-lfs.github.com/spec/v1\noid {OID}\nsize 1\nx y\n");
        assert_eq!(parse(extra.as_bytes()), None);
    }

    #[test]
    fn ls_tree_records_keep_only_regular_blobs() {
        let blob = b"100644 blob 0123abcd     130\tdata/a b.csv";
        assert_eq!(
            candidate(blob).expect("record"),
            Some((&b"data/a b.csv"[..], 130))
        );
        assert_eq!(
            candidate(b"120000 blob 0123abcd      9\tlink").expect("record"),
            None
        );
        assert_eq!(
            candidate(b"160000 commit 0123abcd       -\tsub").expect("record"),
            None
        );
        assert!(candidate(b"garbage").is_err());
    }
}
