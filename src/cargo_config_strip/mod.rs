//! `strip_rustflags`: removing named flags from a tree's own
//! cargo config for the locked window.
//!
//! A foreign checkout's `.cargo/config.toml` can carry a flag that breaks on
//! this host (a `-fuse-ld=lld` with no lld installed, joined by cargo with the
//! user-wide linker choice). Cargo has no way to subtract an array element:
//! `--config` and the environment-variable spellings merge, and `RUSTFLAGS`
//! replaces every config source at once. So while brokkr holds the global
//! lock it puts a filtered copy of the one file cargo reads in `<tree>/.cargo`
//! in place, and restores the original before the lock is released. The rest
//! of the file - `-Dwarnings`, `build.warnings`, `rustdocflags` - keeps
//! applying, which is why this edits rather than moves the file aside.
//!
//! [`matching`] owns which elements are removed; [`transaction`] owns how the
//! file is replaced and recovered. Includes are not followed: only flags
//! physically present in the selected file are edited, and an entry that
//! matched nothing is warned about, naming any include and response file it
//! saw as places the flag might live instead.

pub mod matching;
pub mod transaction;

use std::path::Path;

use crate::error::DevError;
use crate::output;

pub use matching::StripSpec;
pub use transaction::{Installed, has_leftovers, recover, take_over_and_recover, verifies};

/// Put the filtered copy of `dir`'s selected cargo config in place for the
/// locked window, or return `None` when no entry matches anything. The
/// caller has run [`recover`] on `dir` first.
pub fn activate(dir: &Path, specs: &[StripSpec]) -> Result<Option<Installed>, DevError> {
    if specs.is_empty() {
        return Ok(None);
    }
    let cargo_dir = dir.join(".cargo");
    let Some(live) = crate::rustflags::selected_config_file(&cargo_dir)? else {
        output::warn(&format!(
            "strip_rustflags: {} has no cargo config, so nothing was stripped",
            cargo_dir.display()
        ));
        return Ok(None);
    };
    let file = live.display().to_string();
    let meta = std::fs::symlink_metadata(&live)?;
    if meta.file_type().is_symlink() {
        return Err(DevError::Refused(format!(
            "strip_rustflags: {file} is a symlink; brokkr only edits a regular file"
        )));
    }
    let text = std::fs::read_to_string(&live).map_err(|e| {
        DevError::Io(std::io::Error::new(e.kind(), format!("cannot read {file}: {e}")))
    })?;
    let scan = matching::filter_document(&text, specs, &file)?;
    for (spec, count) in specs.iter().zip(&scan.counts) {
        if *count == 0 {
            let mut hint = Vec::new();
            if scan.has_include {
                hint.push("the file has an `include`, which brokkr does not edit");
            }
            if scan.has_response_file {
                hint.push("a rustflags value holds an @response file, which brokkr does not read");
            }
            let hint = if hint.is_empty() {
                String::new()
            } else {
                format!(" ({})", hint.join("; "))
            };
            output::warn(&format!(
                "strip_rustflags {:?} matched nothing in {file}{hint}",
                spec.raw()
            ));
        }
    }
    if scan.text.is_none() {
        return Ok(None);
    }

    let env = crate::rustflags::EffectiveEnv::new(&[]);
    let before = crate::rustflags::effective_rustflags(dir, env).source;
    let installed = transaction::install(&live, |moved| {
        Ok(matching::filter_document(moved, specs, &file)?.text)
    })?;
    if installed.is_some() {
        let removed: usize = scan.counts.iter().sum();
        let noun = if removed == 1 { "match" } else { "matches" };
        output::detail_or_console(&format!(
            "cargo config: strip_rustflags removed {removed} {noun} from {file}"
        ));
        let after = crate::rustflags::effective_rustflags(dir, env).source;
        if before != after {
            output::warn(&format!(
                "strip_rustflags changed which rustflags source cargo uses for a \
                 host build here, from {before} to {after}: a value it emptied no longer \
                 outranks the next source"
            ));
        }
    }
    Ok(installed)
}
