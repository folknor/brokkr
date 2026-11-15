//! rustdoc's lint-only mode for `check`'s rustdoc phase.
//!
//! The phase wants rustdoc's diagnostics, not its output, yet a plain
//! `cargo doc` renders the whole HTML site - pages, search index, static
//! files - and that rendering is most of the phase's wall time. Nightly rustdoc
//! has `-Z unstable-options --check`, which runs the same analysis and lints
//! and writes nothing. Measured on a probe file carrying one each of
//! `broken_intra_doc_links`, `private_intra_doc_links`, `invalid_html_tags`,
//! `bare_urls`, `invalid_codeblock_attributes` and `redundant_explicit_links`:
//! the two modes report the identical six warnings.
//!
//! The flag is unstable, so it is used only when the rustdoc cargo will run
//! lists it in its `-Z unstable-options --help`; a stable toolchain rejects
//! `-Z` and the phase renders as before. The probe runs once per process.
//!
//! The flags reach rustdoc through cargo's rustdocflags, placed by the same
//! decision that places brokkr's rustflags (`rustflags::place` with
//! `FlagKind::Rustdoc`), beside the sweep's own `--cfg` flags: a set env
//! source is appended to, never replaced, so a project's own rustdoc flags
//! survive; otherwise the flags go in as `--config` at whichever of
//! `target."cfg(all())".rustdocflags` and `build.rustdocflags` is live,
//! merged after the project's entries. When the layer is a guess and a target
//! table does outrank it, the injection is inert and the phase renders as
//! before - slower, never wrong.
//!
//! Known gap: a `build.rustdoc` config entry naming another rustdoc is not
//! followed by the probe (`RUSTDOC` in the environment is). Should that
//! rustdoc lack `--check`, it rejects the flag and the phase fails loudly.

use std::path::Path;
use std::sync::OnceLock;

/// The flags that turn rustdoc's rendering off.
pub(crate) const FLAGS: [&str; 2] = ["-Zunstable-options", "--check"];

/// Whether the rustdoc cargo will invoke supports `--check`. `env` is the
/// cargo invocation's extra env, consulted for `RUSTDOC` ahead of the
/// process's own. Cached for the process: one brokkr run drives one toolchain.
pub fn supported(project_root: &Path, env: &[(String, String)]) -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        let program = env
            .iter()
            .find(|(k, _)| k == "RUSTDOC")
            .map(|(_, v)| v.clone())
            .or_else(|| std::env::var("RUSTDOC").ok())
            .unwrap_or_else(|| "rustdoc".into());
        crate::output::run_captured(
            &program,
            &["-Z", "unstable-options", "--help"],
            project_root,
        )
        .is_ok_and(|out| out.success() && help_lists_check(&String::from_utf8_lossy(&out.stdout)))
    })
}

/// Whether rustdoc's help text lists the `--check` option itself - not
/// `--check-cfg` or `--check-theme`.
fn help_lists_check(help: &str) -> bool {
    help.lines().any(|line| {
        let line = line.trim_start();
        line == "--check" || line.starts_with("--check ")
    })
}

/// Make each doc unit the run just documented look up to date to cargo.
///
/// Cargo judges a doc unit fresh by its fingerprint plus its output,
/// `target/doc/<crate>/index.html`: a missing output is dirty ("couldn't read
/// metadata for file"), and so is one older than the package's newest source.
/// `--check` writes no output, so without this every run re-documents every
/// member, changed or not - on a large workspace, slower than rendering.
///
/// Each non-fresh doc artifact cargo reported gets its `index.html` created
/// (a placeholder, only if absent) and its mtime set to `started`, the moment
/// before cargo was launched - not now, so a source edited while rustdoc ran
/// stays newer and the next run re-documents it. A placeholder never passes
/// for real docs: a plain `cargo doc` carries different rustdocflags, so its
/// fingerprint differs and it renders over it.
///
/// Cargo's artifact message for a doc unit carries `"filenames": []`, so the
/// path is rebuilt the way cargo names it: `<doc_dir>/<crate>/index.html`,
/// the crate being the target name with `-` as `_`. The caller passes
/// `<target>/<triple>/doc` for a `[[check]] target` sweep. A wrong guess (an
/// ambient `build.target` triple also moves docs there) leaves the unit dirty -
/// the slow direction, never the wrong one.
pub fn stamp_outputs(stdout: &str, doc_dir: &Path, started: std::time::SystemTime) {
    for name in documented_targets(stdout) {
        let path = doc_dir.join(name.replace('-', "_")).join("index.html");
        let path = path.as_path();
        if !path.exists() {
            if let Some(dir) = path.parent()
                && std::fs::create_dir_all(dir).is_err()
            {
                continue;
            }
            if std::fs::write(path, PLACEHOLDER).is_err() {
                continue;
            }
        }
        // Best effort: a stamp that fails costs one re-documentation next
        // run, never a wrong verdict.
        if let Ok(file) = std::fs::OpenOptions::new().write(true).open(path) {
            file.set_modified(started).ok();
        }
    }
}

const PLACEHOLDER: &str = "<!-- placeholder written by brokkr's rustdoc lint run (rustdoc --check \
renders nothing); run cargo doc to render this crate's documentation -->\n";

/// The target names of the units cargo actually ran (not fresh), from its
/// `--message-format=json` stream. With `--no-deps` every artifact cargo
/// reports for a run is a doc unit; dependencies are only checked, and those
/// arrive as `build-script-executed` or fresh check artifacts.
fn documented_targets(stdout: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("reason").and_then(serde_json::Value::as_str) != Some("compiler-artifact")
            || v.get("fresh").and_then(serde_json::Value::as_bool) != Some(false)
        {
            continue;
        }
        let doc = v
            .get("target")
            .and_then(|t| t.get("doc"))
            .and_then(serde_json::Value::as_bool);
        let name = v
            .get("target")
            .and_then(|t| t.get("name"))
            .and_then(serde_json::Value::as_str);
        let rmeta = v
            .get("filenames")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|f| !f.is_empty());
        if let (Some(true), Some(name), false) = (doc, name, rmeta) {
            out.push(name.to_owned());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_matches_check_but_not_its_prefixed_siblings() {
        assert!(help_lists_check(
            "        --check         Run rustdoc checks\n"
        ));
        assert!(!help_lists_check(
            "        --check-cfg     pass a --check-cfg to rustc\n"
        ));
        assert!(!help_lists_check("        --check-theme FILES\n"));
    }

    #[test]
    fn documented_targets_takes_only_doc_units_that_ran() {
        // The doc-unit shape is cargo's own (`"filenames": []`), captured
        // from a real `cargo doc --message-format=json` run.
        let stdout = concat!(
            r#"{"reason":"compiler-artifact","target":{"name":"brokkr-rustc-guard","doc":true},"filenames":[],"fresh":false}"#,
            "\n",
            r#"{"reason":"compiler-artifact","target":{"name":"fresh-one","doc":true},"filenames":[],"fresh":true}"#,
            "\n",
            r#"{"reason":"compiler-artifact","target":{"name":"libc","doc":true},"filenames":["/t/debug/libc.rmeta"],"fresh":false}"#,
            "\n",
            r#"{"reason":"compiler-artifact","target":{"name":"nodoc","doc":false},"filenames":[],"fresh":false}"#,
            "\n",
            r#"{"reason":"build-finished","success":true}"#,
            "\n",
            "not json\n",
        );
        assert_eq!(
            documented_targets(stdout),
            vec!["brokkr-rustc-guard".to_owned()]
        );
    }
}
