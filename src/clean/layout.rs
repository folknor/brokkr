//! Typed project-state and cleanup layouts.
//!
//! Writers and clean need to agree on these names. A caller that creates a
//! scratch path should use the same layout method that contributes it to the
//! clean plan; clean does not infer ownership by decoding filenames.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::config;

pub(crate) const RUSTFLAGS_TARGET_PREFIX: &str = crate::target_namespace::RUSTFLAGS_PREFIX;

/// The project-local state directory's name, relative to the project root.
pub(crate) const PROJECT_STATE_DIR: &str = ".brokkr";

/// The state directory's own `.gitignore`: everything in it, itself included.
const SELF_IGNORE: &[u8] = b"# Written by brokkr: per-host state.\n*\n";

/// Where script-bench iterations make their directories under a host's
/// resolved scratch `scratch_dir` (`reference/bench-resources.md`, Placement
/// and cleanup).
pub(crate) fn bench_iterations_container(scratch_dir: &Path) -> PathBuf {
    scratch_dir.join("bench-iterations")
}

/// The project-local `.brokkr` root and its named stores.
#[derive(Debug, Clone)]
pub(crate) struct ProjectState {
    root: PathBuf,
}

impl ProjectState {
    pub(crate) fn new(project_root: &Path) -> Self {
        Self {
            root: project_root.join(PROJECT_STATE_DIR),
        }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Create the state directory with a `.gitignore` of `*` inside it, as
    /// pytest, ruff and venv do, so its logs and journals never show up in
    /// `git status` or ride along on a `git add -A` in a repo that does not
    /// ignore it, and do not read as code dirt to check's markdown-only
    /// shortcut. An existing `.gitignore` is left alone. A file git already
    /// tracks (a committed `results.db`) stays tracked under the rule.
    pub(crate) fn ensure_self_ignored(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.root.join(".gitignore"))
        {
            Ok(mut file) => std::io::Write::write_all(&mut file, SELF_IGNORE),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub(crate) fn results_db(&self) -> PathBuf {
        self.root.join("results.db")
    }

    pub(crate) fn sidecar_db(&self) -> PathBuf {
        self.root.join("sidecar.db")
    }

    /// `brokkr crit`'s store: criterion's home and the baseline stamps. Named
    /// `bench` because it predates the verb's rename; it keeps the name so the
    /// baselines already recorded there keep reading.
    pub(crate) fn crit_dir(&self) -> PathBuf {
        self.root.join("bench")
    }

    /// The one place inside the state root a script-bench iteration container
    /// may sit, when the host's scratch is the state root itself
    /// (`reference/bench-resources.md`, Placement and cleanup). Removed by
    /// nothing but the leases: `brokkr clean` has no name for it.
    pub(crate) fn bench_iterations_dir(&self) -> PathBuf {
        bench_iterations_container(&self.root)
    }

    pub(crate) fn check_logs_dir(&self) -> PathBuf {
        self.root.join("check-logs")
    }

    pub(crate) fn accounting_dir(&self) -> PathBuf {
        self.root.join("accounting")
    }

    pub(crate) fn parallel_timings_file(&self) -> PathBuf {
        self.root.join("parallel-timings.toml")
    }

    pub(crate) fn nextest_synth_file(&self) -> PathBuf {
        self.root.join("nextest-synth.toml")
    }

    pub(crate) fn nextest_sweep_env_file(&self) -> PathBuf {
        self.root.join("nextest-sweep-env.toml")
    }

    pub(crate) fn test_hung_dir(&self) -> PathBuf {
        self.root.join("test-hung")
    }

    pub(crate) fn hash_cache_file(&self) -> PathBuf {
        self.root.join("hash_cache")
    }

    /// The flock writers of [`hash_cache_file`](Self::hash_cache_file)
    /// serialise on. Empty; never removed, since a waiter may hold it open.
    pub(crate) fn hash_cache_lock_file(&self) -> PathBuf {
        self.root.join("hash_cache.lock")
    }

    /// litehtml's visual store: mechanical runs, results and approvals.
    pub(crate) fn litehtml_visual_db(&self) -> PathBuf {
        self.root.join("litehtml-visual.db")
    }

    /// sluggrs' visual store: snapshot runs, results and approvals.
    pub(crate) fn sluggrs_visual_db(&self) -> PathBuf {
        self.root.join("sluggrs-visual.db")
    }

    /// The reference cache the retired `brokkr verify` left under the state
    /// root. Nothing writes it now; `brokkr clean` still sweeps it.
    pub(crate) fn verify_reference_dir(&self) -> PathBuf {
        self.root.join("verify-reference")
    }

    pub(crate) fn worktrees_file(&self) -> PathBuf {
        self.root.join("worktrees.toml")
    }

    /// `brokkr build`'s immutable executable snapshots, one directory per
    /// content digest (`artifacts/<xxh128>/<name>`). Outside cargo's target
    /// dir on purpose: every name under `<target>/` is a profile name cargo
    /// may own.
    pub(crate) fn artifacts_dir(&self) -> PathBuf {
        self.root.join("artifacts")
    }

    /// `brokkr serve`'s generation directories, `serve/<name>.<token>/`: the
    /// served executables' own links and the generation's log
    /// (`crate::serve`). Kept apart from `artifacts/` so `brokkr clean`
    /// removing the snapshot store never takes a running server's files.
    pub(crate) fn serve_dir(&self) -> PathBuf {
        self.root.join("serve")
    }

    pub(crate) fn litehtml(&self) -> LitehtmlLayout {
        LitehtmlLayout::new(&self.root)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct LitehtmlLayout {
    prepare_cache: PathBuf,
    capture_staging: PathBuf,
}

impl LitehtmlLayout {
    fn new(state_root: &Path) -> Self {
        Self {
            prepare_cache: state_root.join("prepare-cache"),
            capture_staging: state_root.join("litehtml-capture"),
        }
    }

    pub(crate) fn prepare_cache(&self) -> &Path {
        &self.prepare_cache
    }

    /// Where a Chrome capture is written before it is published into the
    /// fixture's directory; one subdirectory per fixture, empty between runs.
    pub(crate) fn capture_staging(&self) -> &Path {
        &self.capture_staging
    }
}

/// The per-mode ingest output directories the retired nidhogg `nid-ingest`
/// and `suite nidhogg` left in scratch, a whole ingested store each when a
/// run failed. Nothing writes them now; `brokkr clean` still sweeps them.
pub(crate) struct NidhoggLayout<'a> {
    paths: &'a config::ResolvedPaths,
}

impl<'a> NidhoggLayout<'a> {
    pub(crate) fn new(paths: &'a config::ResolvedPaths) -> Self {
        Self { paths }
    }

    pub(crate) fn ingest_outputs(&self) -> [PathBuf; 3] {
        ["run", "bench", "hotpath"]
            .map(|mode| self.paths.scratch_dir.join(format!("{mode}-ingest-output")))
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum PbfhoggFileKind {
    Pbf,
    Osc,
    Text,
}

/// The retired pbfhogg commands' known scratch outputs, kept for cleanup;
/// arbitrary `.pbf`/`.osc.gz` files in scratch are not targets.
pub(crate) struct PbfhoggLayout<'a> {
    paths: &'a config::ResolvedPaths,
}

impl<'a> PbfhoggLayout<'a> {
    pub(crate) fn new(paths: &'a config::ResolvedPaths) -> Self {
        Self { paths }
    }

    /// Where clean looks for the output the retired pbfhogg verify left: under `target_dir`,
    /// cargo's resolved target dir, where it wrote; a host `target` never applied to it.
    pub(crate) fn verify_output(target_dir: &Path) -> PathBuf {
        target_dir.join(crate::target_namespace::VERIFY)
    }

    pub(crate) fn scratch(&self) -> &Path {
        &self.paths.scratch_dir
    }

    pub(crate) fn fixed_scratch_file(scratch_dir: &Path, name: &str) -> PathBuf {
        scratch_dir.join(name)
    }

    pub(crate) fn command_output_file(
        scratch_dir: &Path,
        prefix: &str,
        command: &str,
        kind: PbfhoggFileKind,
    ) -> PathBuf {
        let extension = match kind {
            PbfhoggFileKind::Pbf => "osm.pbf",
            PbfhoggFileKind::Osc => "osc.gz",
            PbfhoggFileKind::Text => "txt",
        };
        scratch_dir.join(format!("{prefix}-{command}-output.{extension}"))
    }

    /// `<scratch>/library`: the run directories the retired `brokkr library`
    /// left, one `<scenario>/run-N/` each. Nothing writes them now;
    /// `brokkr clean` still sweeps them.
    pub(crate) fn library_runs(scratch_dir: &Path) -> PathBuf {
        scratch_dir.join("library")
    }

    /// Every directory under [`library_runs`](Self::library_runs) that
    /// holds runs, and the run directories in each, in name order.
    pub(crate) fn library_run_dirs(&self) -> Vec<(PathBuf, Vec<PathBuf>)> {
        let list = |dir: &Path| -> Vec<PathBuf> {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return Vec::new();
            };
            let mut dirs: Vec<PathBuf> = entries
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.path())
                .collect();
            dirs.sort();
            dirs
        };
        list(&Self::library_runs(self.scratch()))
            .into_iter()
            .map(|scenario| {
                let runs = list(&scenario);
                (scenario, runs)
            })
            .collect()
    }

    pub(crate) fn multi_extract_dir(scratch_dir: &Path) -> PathBuf {
        scratch_dir.join("multi-extract")
    }

    pub(crate) fn scratch_dir_with_dataset(
        scratch_dir: &Path,
        prefix: &str,
        dataset: &str,
    ) -> PathBuf {
        scratch_dir.join(format!("{prefix}-{dataset}"))
    }

    pub(crate) fn geocode_dir(scratch_dir: &Path, dataset: &str) -> PathBuf {
        Self::scratch_dir_with_dataset(scratch_dir, "geocode", dataset)
    }

    pub(crate) fn merged_pbf_cache(
        scratch_dir: &Path,
        pbf_path: &Path,
        snapshot_key: &str,
        osc_seq: &str,
    ) -> PathBuf {
        let stem = pbf_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("input");
        scratch_dir.join(format!(
            "{stem}-snap{snapshot_key}-osc{osc_seq}-bench-merged.osm.pbf"
        ))
    }

    pub(crate) fn scratch_files(&self) -> Vec<PathBuf> {
        const PBF_COMMANDS: &[&str] = &[
            "sort",
            "cat",
            "tags-filter",
            "getid",
            "getparents",
            "renumber",
            "apply-changes",
            "add-locations-to-ways",
            "time-filter",
            "repack",
            "degrade",
            "extract",
        ];
        const OSC_COMMANDS: &[&str] = &["tags-filter", "merge-changes", "diff", "diff-snapshots"];
        const TEXT_COMMANDS: &[&str] = &["diff", "diff-snapshots"];

        let scratch_dir = self.scratch();
        let mut files = BTreeSet::new();
        // The retired typed commands did not create a `.pbfhogg-external-join-*`
        // directory, so clean does not reclaim that unconstructed namespace.
        for name in [
            "bench-merge-output.osm.pbf",
            "bench-blob-filter-output.osm.pbf",
            "bench-osmium-altw-output.osm.pbf",
            "multi-extract-config.json",
        ] {
            files.insert(Self::fixed_scratch_file(scratch_dir, name));
        }
        for prefix in ["bench", "hotpath"] {
            for command in PBF_COMMANDS {
                files.insert(Self::command_output_file(
                    scratch_dir,
                    prefix,
                    command,
                    PbfhoggFileKind::Pbf,
                ));
            }
            for command in OSC_COMMANDS {
                files.insert(Self::command_output_file(
                    scratch_dir,
                    prefix,
                    command,
                    PbfhoggFileKind::Osc,
                ));
            }
            for command in TEXT_COMMANDS {
                files.insert(Self::command_output_file(
                    scratch_dir,
                    prefix,
                    command,
                    PbfhoggFileKind::Text,
                ));
            }
        }

        self.add_merged_pbf_caches(&mut files);
        files.into_iter().collect()
    }

    pub(crate) fn scratch_dirs(&self) -> Vec<PathBuf> {
        let scratch_dir = self.scratch();
        let mut dirs = BTreeSet::new();
        // The fixed-name private directory an older brokkr gave pbfhogg
        // children, beside their outputs: on a host that ran one, a killed
        // child's staging there can be a whole PBF. Today's per-launch ones
        // go with their leases (`crate::reservations`), not from here.
        dirs.insert(scratch_dir.join(".brokkr-pbfhogg"));
        dirs.insert(self.paths.data_dir.join(".brokkr-pbfhogg"));
        dirs.insert(Self::multi_extract_dir(scratch_dir));
        for dataset in self.paths.datasets.keys() {
            dirs.insert(Self::geocode_dir(scratch_dir, dataset));
        }
        dirs.into_iter().collect()
    }

    fn add_merged_pbf_caches(&self, files: &mut BTreeSet<PathBuf>) {
        let mut configured_inputs = BTreeSet::new();
        for dataset in self.paths.datasets.values() {
            for pbf in dataset.pbf.values() {
                configured_inputs.insert(self.paths.data_dir.join(&pbf.file));
            }
            for osc in dataset.osc.values() {
                configured_inputs.insert(self.paths.data_dir.join(&osc.file));
            }
            for snapshot in dataset.snapshot.values() {
                for pbf in snapshot.pbf.values() {
                    configured_inputs.insert(self.paths.data_dir.join(&pbf.file));
                }
                for osc in snapshot.osc.values() {
                    configured_inputs.insert(self.paths.data_dir.join(&osc.file));
                }
            }
        }

        for dataset in self.paths.datasets.values() {
            self.add_merged_names(
                files,
                &configured_inputs,
                "base",
                &dataset.pbf,
                &dataset.osc,
            );
            for (key, snapshot) in &dataset.snapshot {
                // Snapshot PBFs can be diffed with their own OSC chain. A
                // union of the configured chains also covers the resolver's
                // base-OSC fallback without trying to decode a filename.
                self.add_merged_names(files, &configured_inputs, key, &snapshot.pbf, &dataset.osc);
                self.add_merged_names(files, &configured_inputs, key, &snapshot.pbf, &snapshot.osc);
            }
        }
    }

    fn add_merged_names(
        &self,
        files: &mut BTreeSet<PathBuf>,
        configured_inputs: &BTreeSet<PathBuf>,
        snapshot_key: &str,
        pbfs: &HashMap<String, config::PbfEntry>,
        oscs: &HashMap<String, config::OscEntry>,
    ) {
        let scratch_dir = self.scratch();
        for pbf in pbfs.values() {
            for seq in oscs.keys() {
                let candidate =
                    Self::merged_pbf_cache(scratch_dir, Path::new(&pbf.file), snapshot_key, seq);
                if !configured_inputs.contains(&candidate) {
                    files.insert(candidate);
                }
            }
        }
    }
}

/// A name-prefix namespace under Cargo's target directory. Old hashes cannot
/// be reconstructed after a sweep changes, so the reserved namespace is the
/// identity of these isolated targets.
pub(crate) struct PrefixedTargets<'a> {
    target_root: &'a Path,
    prefix: &'a str,
}

impl<'a> PrefixedTargets<'a> {
    /// The directories under `target_root` in the reserved `prefix`
    /// namespace (`crate::target_namespace`).
    pub(crate) fn new(target_root: &'a Path, prefix: &'a str) -> Self {
        Self {
            target_root,
            prefix,
        }
    }

    /// The isolated per-RUSTFLAGS sweep targets.
    pub(crate) fn rustflags(target_root: &'a Path) -> Self {
        Self::new(target_root, RUSTFLAGS_TARGET_PREFIX)
    }

    pub(crate) fn existing_dirs(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(self.target_root) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| {
                let is_reserved = entry
                    .file_name()
                    .to_string_lossy()
                    .strip_prefix(self.prefix)
                    .is_some_and(|rest| !rest.is_empty());
                (is_reserved && entry.file_type().ok()?.is_dir()).then(|| entry.path())
            })
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// The state directory ignores itself, and a `.gitignore` already there,
    /// written by brokkr or by hand, is never rewritten.
    #[test]
    fn the_state_directory_ignores_itself_once() {
        let root = crate::test_scratch::scratch("layout", "self_ignore");
        let state = ProjectState::new(&root);
        state.ensure_self_ignored().unwrap();
        let ignore = state.root().join(".gitignore");
        assert_eq!(std::fs::read(&ignore).unwrap(), SELF_IGNORE);
        std::fs::write(&ignore, b"custom\n").unwrap();
        state.ensure_self_ignored().unwrap();
        assert_eq!(std::fs::read(&ignore).unwrap(), b"custom\n");
    }
}
