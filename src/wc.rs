//! `brokkr wc` - list rust source files above a line-count threshold.
//!
//! A convenience wrapper: scans tracked and untracked-not-ignored `.rs`
//! files (via `git ls-files`, so `target/` and other gitignored output is
//! excluded) and prints those over the threshold, largest first.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::DevError;
use crate::output;

/// Default threshold: files with strictly more lines than this are listed.
pub const DEFAULT_THRESHOLD: usize = 800;

struct Entry {
    path: PathBuf,
    lines: usize,
}

pub fn run(project_root: &Path, threshold: usize) -> Result<(), DevError> {
    let mut entries: Vec<Entry> = Vec::new();
    for rel in rust_files(project_root)? {
        let abs = project_root.join(&rel);
        let Ok(content) = std::fs::read_to_string(&abs) else {
            continue;
        };
        let lines = content.lines().count();
        if lines > threshold {
            entries.push(Entry { path: rel, lines });
        }
    }

    if entries.is_empty() {
        output::wc_msg(&format!("no rust files over {threshold} lines"));
        return Ok(());
    }

    // Largest first; ties broken by path for a stable order.
    entries.sort_by(|a, b| b.lines.cmp(&a.lines).then_with(|| a.path.cmp(&b.path)));

    let width = entries
        .iter()
        .map(|e| count_digits(e.lines))
        .max()
        .unwrap_or(0)
        .max("lines".len());

    println!("{:>width$}  file", "lines");
    for e in &entries {
        println!("{:>width$}  {}", e.lines, e.path.display());
    }
    output::wc_msg(&format!("{} file(s) over {threshold} lines", entries.len()));

    Ok(())
}

fn count_digits(n: usize) -> usize {
    if n == 0 {
        return 1;
    }
    let mut digits = 0usize;
    let mut v = n;
    while v > 0 {
        digits += 1;
        v /= 10;
    }
    digits
}

/// Tracked + untracked-not-ignored `.rs` files, via `git ls-files`.
fn rust_files(project_root: &Path) -> Result<Vec<PathBuf>, DevError> {
    let output = Command::new("git")
        .args(["ls-files", "-z", "--cached", "--others", "--exclude-standard"])
        .current_dir(project_root)
        .output()
        .map_err(DevError::Io)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(DevError::Subprocess {
            program: "git ls-files".into(),
            code: output.status.code(),
            stderr,
        });
    }
    let mut files = Vec::new();
    for raw in output.stdout.split(|b| *b == 0) {
        if raw.is_empty() {
            continue;
        }
        if let Ok(s) = std::str::from_utf8(raw) {
            let path = PathBuf::from(s);
            if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                files.push(path);
            }
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_digits_matches_decimal_length() {
        assert_eq!(count_digits(0), 1);
        assert_eq!(count_digits(9), 1);
        assert_eq!(count_digits(10), 2);
        assert_eq!(count_digits(800), 3);
        assert_eq!(count_digits(1234), 4);
    }
}
