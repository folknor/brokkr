//! Workload registry resolution: a name -> a verified, frozen invocation.

use std::path::Path;

use crate::config::{CORPUS_TOKEN, DevConfig, MogwaiConfig, MogwaiTiming, MogwaiWorkload};
use crate::error::DevError;
use crate::output;
use crate::preflight;

/// A workload resolved from the registry, ready to run.
#[derive(Debug)]
pub(crate) struct Resolved<'a> {
    /// The registered name, and the label the run is filed under in
    /// `results.db`.
    pub(crate) name: String,
    /// The frozen entry, borrowed from the parsed config.
    pub(crate) entry: &'a MogwaiWorkload,
    /// The argv with `{corpus}` already substituted. Identical to
    /// `entry.args` for a generated workload.
    args: Vec<String>,
}

impl Resolved<'_> {
    /// The argv passed to the mogwai binary, as borrowed `&str`s.
    pub(crate) fn args(&self) -> Vec<&str> {
        self.args.iter().map(String::as_str).collect()
    }

    /// The corpus name this workload reads, if any.
    ///
    /// This is what a corpus row's `input_file` carries - the registry KEY, not
    /// the resolved path. The path is per-host; the key is not, and filing rows
    /// under a path would make the same measurement on two machines look like
    /// two different benchmarks to `--compare`, whose pairing key includes it.
    pub(crate) fn corpus(&self) -> Option<&str> {
        self.entry.corpus.as_deref()
    }
}

/// Fetch the `[mogwai]` block, with an actionable error when it's absent.
pub(crate) fn config(dev_config: &DevConfig) -> Result<&MogwaiConfig, DevError> {
    dev_config.mogwai.as_ref().ok_or_else(|| {
        DevError::Config(
            "no [mogwai] section in brokkr.toml - `brokkr mogwai` needs the CLI \
             package and at least one frozen workload:\n\n  [mogwai]\n  \
             package = \"mogwai-cli\"\n\n  [mogwai.workloads.screen-probe]\n  \
             description = \"...\"\n  args = [\"screen\", \"--preset\", \"...\"]\n  \
             runs = 3"
                .into(),
        )
    })
}

/// Resolve a workload name against the registry.
///
/// Retired entries (those carrying a `successor`) refuse to run. The refusal is
/// the whole point of recording the pointer: the alternative is a name that
/// still works but no longer measures what its historical rows measured, which
/// is the one failure this registry exists to prevent.
pub(crate) fn resolve<'a>(
    dev_config: &'a DevConfig,
    hostname: &str,
    project_root: &Path,
    name: &str,
) -> Result<Resolved<'a>, DevError> {
    let cfg = config(dev_config)?;
    let entry = cfg.workload(name)?;

    if let Some(successor) = &entry.successor {
        return Err(DevError::Config(format!(
            "workload {name:?} is retired - its invocation changed, and it was \
             replaced by {successor:?}.\n  its rows are still queryable \
             (`brokkr results --command {name}`), but a new run under this name \
             would not be comparable to them.\n  run the successor instead: \
             `brokkr mogwai {successor}`"
        )));
    }

    // Generated workloads are self-contained; only a corpus workload needs a
    // host lookup, so a machine holding no deliveries can still run three of
    // the four critical-path workloads without registering anything.
    let args = match &entry.corpus {
        None => entry.args.clone(),
        Some(corpus) => {
            let path = resolve_corpus(dev_config, hostname, project_root, name, corpus)?;
            entry
                .args
                .iter()
                .map(|a| a.replace(CORPUS_TOKEN, &path))
                .collect()
        }
    };

    Ok(Resolved {
        name: name.to_owned(),
        entry,
        args,
    })
}

/// Resolve a corpus name to a verified absolute path on this host.
fn resolve_corpus(
    dev_config: &DevConfig,
    hostname: &str,
    project_root: &Path,
    workload: &str,
    corpus: &str,
) -> Result<String, DevError> {
    let host = dev_config.hosts.get(hostname).ok_or_else(|| {
        DevError::Config(format!(
            "workload {workload:?} reads corpus {corpus:?}, but this host \
             ({hostname}) has no section in brokkr.toml.\n  add:\n\n  \
             [{hostname}.corpus.{corpus}]\n  path = \"...\"\n  xxh128 = \"...\""
        ))
    })?;

    let entry = host.corpus.get(corpus).ok_or_else(|| {
        let known: Vec<&str> = host.corpus.keys().map(String::as_str).collect();
        let known = if known.is_empty() {
            "none registered".to_owned()
        } else {
            known.join(", ")
        };
        DevError::Config(format!(
            "workload {workload:?} reads corpus {corpus:?}, which is not \
             registered for this host ({hostname}).\n  registered here: \
             {known}\n  add it as [{hostname}.corpus.{corpus}]"
        ))
    })?;

    let path = entry.resolve_path(project_root);

    if !path.exists() {
        return Err(DevError::Config(format!(
            "corpus {corpus:?} not found at {}\n  registered as: {}\n  \
             origin: [{hostname}.corpus.{corpus}].path",
            path.display(),
            entry.path.display(),
        )));
    }

    // No registered digest means the archive cannot be checked for drift. Say
    // so on every run rather than refusing: the digest is XXH128, which no
    // delivery manifest carries, so it has to be read off this machine before
    // it can be registered - and `brokkr env` is where it is read off.
    match &entry.xxh128 {
        Some(expected) => {
            let origin = format!("[{hostname}.corpus.{corpus}].xxh128 in brokkr.toml");
            preflight::verify_file_hash(&path, expected, project_root, Some(&origin))?;
        }
        None => {
            output::warn(&format!(
                "corpus {corpus:?} has no xxh128 registered - running UNVERIFIED. \
                 `brokkr env` prints the digest to paste into \
                 [{hostname}.corpus.{corpus}]."
            ));
        }
    }

    path.to_str().map(str::to_owned).ok_or_else(|| {
        DevError::Config(format!(
            "corpus path is not valid UTF-8: {}",
            path.display()
        ))
    })
}

/// Render the registry as the bare `brokkr mogwai` index.
///
/// Retired entries are listed too, marked with their successor - a name that
/// appears in months-old notes should be findable here rather than look like it
/// was deleted.
pub(crate) fn format_index(cfg: &MogwaiConfig) -> String {
    if cfg.workloads.is_empty() {
        return "no workloads registered - add [mogwai.workloads.<name>] to brokkr.toml"
            .to_owned();
    }

    let width = cfg.workloads.keys().map(String::len).max().unwrap_or(0);
    let mut out = String::new();
    for (name, workload) in &cfg.workloads {
        let detail = match (&workload.successor, &workload.description) {
            (Some(successor), _) => format!("retired -> {successor}"),
            (None, Some(description)) => description.clone(),
            (None, None) => String::new(),
        };
        // The cost of a baseline refresh, legible before it is paid: N runs of
        // a month-long walk is an evening, not a coffee break.
        let cost = match (workload.runs, workload.expect_seconds) {
            (Some(n), Some(s)) => format!(" [{n} run(s), ~{s}s each]"),
            (Some(n), None) => format!(" [{n} run(s)]"),
            (None, Some(s)) => format!(" [~{s}s each]"),
            (None, None) => String::new(),
        };
        // Only the deviation is worth a column: external is the default and
        // marking every row with it would bury the one entry that differs.
        let clock = if workload.timing == MogwaiTiming::SelfReported {
            " [self-reported]"
        } else {
            ""
        };
        out.push_str(&format!("  {name:<width$}  {detail}{cost}{clock}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    /// A fresh scratch dir under the crate's gitignored `target/`
    /// (project rules forbid `/tmp`).
    fn tmpdir(test_name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-tmp/mogwai")
            .join(test_name);
        if dir.exists() {
            fs::remove_dir_all(&dir).unwrap();
        }
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn config_with(toml_src: &str) -> DevConfig {
        let dir = tmpdir("cfg");
        fs::write(dir.join("brokkr.toml"), toml_src).unwrap();
        crate::config::load(&dir).unwrap().1
    }

    /// Load a config into an EXISTING dir. Takes the dir rather than a name
    /// because a corpus test writes its archive first and `tmpdir` wipes.
    fn config_in(dir: &Path, toml_src: &str) -> DevConfig {
        fs::write(dir.join("brokkr.toml"), toml_src).unwrap();
        crate::config::load(dir).unwrap().1
    }

    const BASE: &str = "project = \"mogwai\"\n\n[mogwai]\npackage = \"mogwai-cli\"\n\n";

    /// Hostname and root for the generated-workload tests, which never consult
    /// either - only a corpus workload triggers a host lookup.
    fn anyhost() -> (&'static str, &'static Path) {
        ("nohost", Path::new("."))
    }

    #[test]
    fn resolves_a_registered_workload() {
        let cfg = config_with(&format!(
            "{BASE}[mogwai.workloads.screen-probe]\nargs = [\"screen\", \"--probe\"]\n"
        ));
        let (host, root) = anyhost();
        let resolved = resolve(&cfg, host, root, "screen-probe").unwrap();
        assert_eq!(resolved.name, "screen-probe");
        assert_eq!(resolved.args(), vec!["screen", "--probe"]);
    }

    #[test]
    fn retired_workload_refuses_and_names_its_successor() {
        let cfg = config_with(&format!(
            "{BASE}[mogwai.workloads.screen-v1]\nargs = [\"screen\"]\n\
             successor = \"screen-v2\"\n\n\
             [mogwai.workloads.screen-v2]\nargs = [\"screen\", \"--wide\"]\n"
        ));
        let (host, root) = anyhost();
        let err = resolve(&cfg, host, root, "screen-v1").unwrap_err();
        let DevError::Config(msg) = err else {
            panic!("expected DevError::Config, got {err:?}");
        };
        // The refusal must point at the heir and say the old rows survive,
        // otherwise the pointer buys nothing over deleting the entry.
        assert!(msg.contains("screen-v2"), "{msg}");
        assert!(msg.contains("brokkr results"), "{msg}");
    }

    #[test]
    fn unknown_workload_lists_only_the_live_names() {
        let cfg = config_with(&format!(
            "{BASE}[mogwai.workloads.alpha]\nargs = [\"a\"]\n\n\
             [mogwai.workloads.beta]\nargs = [\"b\"]\nsuccessor = \"alpha\"\n"
        ));
        let (host, root) = anyhost();
        let err = resolve(&cfg, host, root, "gamma").unwrap_err();
        let DevError::Config(msg) = err else {
            panic!("expected DevError::Config, got {err:?}");
        };
        assert!(msg.contains("alpha"), "{msg}");
        // Suggesting a retired name would send the reader to a second refusal.
        assert!(!msg.contains("beta"), "{msg}");
    }

    #[test]
    fn measurement_flags_are_rejected_at_parse_time() {
        let dir = tmpdir("mode_flag");
        fs::write(
            dir.join("brokkr.toml"),
            format!("{BASE}[mogwai.workloads.w]\nargs = [\"screen\", \"--bench\"]\n"),
        )
        .unwrap();
        let err = crate::config::load(&dir).unwrap_err();
        let DevError::Config(msg) = err else {
            panic!("expected DevError::Config, got {err:?}");
        };
        assert!(msg.contains("--bench"), "{msg}");
    }

    #[test]
    fn successor_pointing_nowhere_is_rejected() {
        let dir = tmpdir("dangling");
        fs::write(
            dir.join("brokkr.toml"),
            format!("{BASE}[mogwai.workloads.w]\nargs = [\"a\"]\nsuccessor = \"ghost\"\n"),
        )
        .unwrap();
        let err = crate::config::load(&dir).unwrap_err();
        let DevError::Config(msg) = err else {
            panic!("expected DevError::Config, got {err:?}");
        };
        assert!(msg.contains("ghost"), "{msg}");
    }

    #[test]
    fn empty_args_are_rejected() {
        let dir = tmpdir("empty_args");
        fs::write(
            dir.join("brokkr.toml"),
            format!("{BASE}[mogwai.workloads.w]\nargs = []\n"),
        )
        .unwrap();
        let err = crate::config::load(&dir).unwrap_err();
        let DevError::Config(msg) = err else {
            panic!("expected DevError::Config, got {err:?}");
        };
        assert!(msg.contains("stated in full"), "{msg}");
    }

    // -----------------------------------------------------------------------
    // Corpus workloads
    // -----------------------------------------------------------------------

    const ARCHIVE: &str = "delivered bytes\n";

    /// Write the archive and return its digest, computed the way
    /// `verify_file_hash` does - so the test pins behaviour, not a literal.
    fn write_archive(root: &Path, rel: &str) -> String {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, ARCHIVE).unwrap();
        preflight::compute_xxh128(&path).unwrap()
    }

    #[test]
    fn corpus_token_is_replaced_with_the_resolved_path() {
        let root = tmpdir("corpus_ok");
        let digest = write_archive(&root, "deliveries/july.bin");
        let cfg = config_in(
            &root,
            &format!(
                "{BASE}[mogwai.workloads.measure12a]\n\
                 args = [\"measure\", \"--input\", \"{{corpus}}\"]\n\
                 corpus = \"july\"\n\n\
                 [frigg.corpus.july]\npath = \"deliveries/july.bin\"\n\
                 xxh128 = \"{digest}\"\n"
            ),
        );

        let resolved = resolve(&cfg, "frigg", &root, "measure12a").unwrap();
        let args = resolved.args();
        let expected = root.join("deliveries/july.bin");
        assert_eq!(args, vec!["measure", "--input", expected.to_str().unwrap()]);
        // The row files under the registry key, not the host-specific path.
        assert_eq!(resolved.corpus(), Some("july"));
    }

    /// A digest nobody has read off this machine yet must not block the first
    /// run - `brokkr env` is where it gets read, and that comes after the entry
    /// exists. The run is unverified and says so; only a WRONG digest refuses.
    #[test]
    fn corpus_without_a_registered_digest_still_resolves() {
        let root = tmpdir("corpus_nodigest");
        write_archive(&root, "july.bin");
        let cfg = config_in(
            &root,
            &format!(
                "{BASE}[mogwai.workloads.w]\nargs = [\"m\", \"{{corpus}}\"]\n\
                 corpus = \"july\"\n\n\
                 [frigg.corpus.july]\npath = \"july.bin\"\n"
            ),
        );

        let resolved = resolve(&cfg, "frigg", &root, "w").unwrap();
        let expected = root.join("july.bin");
        assert_eq!(resolved.args(), vec!["m", expected.to_str().unwrap()]);
    }

    #[test]
    fn corpus_refuses_an_archive_that_drifted_since_registration() {
        let root = tmpdir("corpus_drift");
        write_archive(&root, "july.bin");
        let cfg = config_in(
            &root,
            &format!(
                "{BASE}[mogwai.workloads.w]\nargs = [\"m\", \"{{corpus}}\"]\n\
                 corpus = \"july\"\n\n\
                 [frigg.corpus.july]\npath = \"july.bin\"\n\
                 xxh128 = \"{}\"\n",
                "0".repeat(32)
            ),
        );
        fs::write(root.join("july.bin"), ARCHIVE).unwrap();

        let err = resolve(&cfg, "frigg", &root, "w").unwrap_err();
        let DevError::Preflight(msgs) = err else {
            panic!("expected a preflight refusal, got {err:?}");
        };
        let joined = msgs.join("\n");
        assert!(joined.contains("hash mismatch"), "{joined}");
        // Naming the registration is what makes the fix "re-register
        // deliberately" rather than "why is this path wrong".
        assert!(joined.contains("[frigg.corpus.july]"), "{joined}");
    }

    #[test]
    fn corpus_unregistered_on_this_host_says_what_to_add() {
        let root = tmpdir("corpus_nohost");
        let cfg = config_in(
            &root,
            &format!(
                "{BASE}[mogwai.workloads.w]\nargs = [\"m\", \"{{corpus}}\"]\n\
                 corpus = \"july\"\n"
            ),
        );
        let err = resolve(&cfg, "unknownbox", &root, "w").unwrap_err();
        let DevError::Config(msg) = err else {
            panic!("expected DevError::Config, got {err:?}");
        };
        assert!(msg.contains("unknownbox.corpus.july"), "{msg}");
    }

    #[test]
    fn generated_workload_never_consults_the_host() {
        // The point of the split: a machine holding no deliveries can still run
        // every generated workload without registering anything.
        let cfg = config_with(&format!(
            "{BASE}[mogwai.workloads.w]\nargs = [\"screen\"]\n"
        ));
        assert!(resolve(&cfg, "a-host-that-does-not-exist", Path::new("/nonexistent"), "w").is_ok());
    }

    #[test]
    fn corpus_without_a_token_is_rejected() {
        let dir = tmpdir("corpus_no_token");
        fs::write(
            dir.join("brokkr.toml"),
            format!("{BASE}[mogwai.workloads.w]\nargs = [\"m\"]\ncorpus = \"july\"\n"),
        )
        .unwrap();
        let err = crate::config::load(&dir).unwrap_err();
        let DevError::Config(msg) = err else {
            panic!("expected DevError::Config, got {err:?}");
        };
        assert!(msg.contains("{corpus}"), "{msg}");
    }

    #[test]
    fn token_without_a_corpus_is_rejected() {
        let dir = tmpdir("token_no_corpus");
        fs::write(
            dir.join("brokkr.toml"),
            format!("{BASE}[mogwai.workloads.w]\nargs = [\"m\", \"{{corpus}}\"]\n"),
        )
        .unwrap();
        let err = crate::config::load(&dir).unwrap_err();
        let DevError::Config(msg) = err else {
            panic!("expected DevError::Config, got {err:?}");
        };
        // Otherwise the literal "{corpus}" reaches the child as a filename.
        assert!(msg.contains("literally"), "{msg}");
    }

    #[test]
    fn self_reported_timing_without_a_reason_is_rejected() {
        let dir = tmpdir("timing_no_reason");
        fs::write(
            dir.join("brokkr.toml"),
            format!(
                "{BASE}[mogwai.workloads.w]\nargs = [\"m\"]\n\
                 timing = \"self_reported\"\n"
            ),
        )
        .unwrap();
        let err = crate::config::load(&dir).unwrap_err();
        let DevError::Config(msg) = err else {
            panic!("expected DevError::Config, got {err:?}");
        };
        assert!(msg.contains("timing_reason"), "{msg}");
    }

    #[test]
    fn self_reported_timing_with_a_reason_is_accepted() {
        let cfg = config_with(&format!(
            "{BASE}[mogwai.workloads.w]\nargs = [\"m\"]\n\
             timing = \"self_reported\"\n\
             timing_reason = \"the corpus hash pass is not the engine\"\n"
        ));
        let resolved = resolve(&cfg, "nohost", Path::new("."), "w").unwrap();
        assert_eq!(resolved.entry.timing, MogwaiTiming::SelfReported);
    }

    /// The default has to be external, or a registry that says nothing about
    /// timing would quietly forfeit the back-fill that makes retroactive
    /// baselines possible.
    #[test]
    fn timing_defaults_to_external() {
        let cfg = config_with(&format!("{BASE}[mogwai.workloads.w]\nargs = [\"m\"]\n"));
        let resolved = resolve(&cfg, "nohost", Path::new("."), "w").unwrap();
        assert_eq!(resolved.entry.timing, MogwaiTiming::External);
    }

    #[test]
    fn index_marks_the_self_reported_entry_only() {
        let cfg = config_with(&format!(
            "{BASE}[mogwai.workloads.alpha]\nargs = [\"a\"]\n\n\
             [mogwai.workloads.beta]\nargs = [\"b\"]\n\
             timing = \"self_reported\"\ntiming_reason = \"setup dominates\"\n"
        ));
        let index = format_index(config(&cfg).unwrap());
        let alpha = index.lines().find(|l| l.contains("alpha")).unwrap();
        let beta = index.lines().find(|l| l.contains("beta")).unwrap();
        assert!(!alpha.contains("self-reported"), "{index}");
        assert!(beta.contains("self-reported"), "{index}");
    }

    #[test]
    fn index_marks_retired_entries() {
        let cfg = config_with(&format!(
            "{BASE}[mogwai.workloads.alpha]\nargs = [\"a\"]\n\
             description = \"the live one\"\n\n\
             [mogwai.workloads.beta]\nargs = [\"b\"]\nsuccessor = \"alpha\"\n"
        ));
        let index = format_index(config(&cfg).unwrap());
        assert!(index.contains("the live one"), "{index}");
        assert!(index.contains("retired -> alpha"), "{index}");
    }
}
