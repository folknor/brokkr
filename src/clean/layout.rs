//! Typed project-state and cleanup layouts.
//!
//! Writers and clean need to agree on these names. A caller that creates a
//! scratch path should use the same layout method that contributes it to the
//! clean plan; clean does not infer ownership by decoding filenames.

use std::collections::{BTreeSet, HashMap};
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};

use crate::config;

pub(crate) const RUSTFLAGS_TARGET_PREFIX: &str = crate::target_namespace::RUSTFLAGS_PREFIX;

/// The project-local state directory's name, relative to the project root.
pub(crate) const PROJECT_STATE_DIR: &str = ".brokkr";

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

    pub(crate) fn results_db(&self) -> PathBuf {
        self.root.join("results.db")
    }

    pub(crate) fn sidecar_db(&self) -> PathBuf {
        self.root.join("sidecar.db")
    }

    pub(crate) fn bench_dir(&self) -> PathBuf {
        self.root.join("bench")
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

    /// Which pbfhogg builds accept which flags, keyed by executable digest.
    pub(crate) fn pbfhogg_capabilities_file(&self) -> PathBuf {
        self.root.join("pbfhogg-capabilities")
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

    /// Spill directory for the corpus digest's external sort. Each sorter
    /// makes its own subdirectory here and removes it when dropped.
    pub(crate) fn corpus_sort_dir(&self) -> PathBuf {
        self.root.join("corpus-sort")
    }

    pub(crate) fn nidhogg_pid_file(&self) -> PathBuf {
        self.root.join("nidhogg.pid")
    }

    pub(crate) fn ratatoskr(&self) -> RatatoskrLayout {
        RatatoskrLayout::new(&self.root)
    }

    pub(crate) fn piners(&self) -> PinersLayout {
        PinersLayout::new(&self.root)
    }

    pub(crate) fn dellingr(&self) -> DellingrLayout {
        DellingrLayout::new(&self.root)
    }

    pub(crate) fn mogwai(&self) -> MogwaiLayout {
        MogwaiLayout::new(&self.root)
    }

    pub(crate) fn litehtml(&self) -> LitehtmlLayout {
        LitehtmlLayout::new(&self.root)
    }
}

/// Ratatoskr run artifacts and the durable gate store.
#[derive(Debug, Clone)]
pub(crate) struct RatatoskrLayout {
    root: PathBuf,
}

impl RatatoskrLayout {
    fn new(state_root: &Path) -> Self {
        Self {
            root: state_root.join("ratatoskr"),
        }
    }

    pub(crate) fn gate_db(&self) -> PathBuf {
        self.root.join("gate.db")
    }

    pub(crate) fn artifact_roots(&self) -> [PathBuf; 4] {
        [
            self.service_artifacts(),
            self.service_mock_artifacts(),
            self.sync_artifacts(),
            self.mock_artifacts(),
        ]
    }

    /// Service runs have a dedicated level so script stems cannot collide
    /// with sync artifacts, mock state, or the gate database.
    pub(crate) fn service_artifacts(&self) -> PathBuf {
        self.root.join("service")
    }

    /// Fixture state for service runs stays separate from both service
    /// artifacts and mock-serve's stable readiness directory.
    pub(crate) fn service_mock_artifacts(&self) -> PathBuf {
        self.root.join("service-mock")
    }

    /// Per-test artifacts for sync runs.
    pub(crate) fn sync_artifacts(&self) -> PathBuf {
        self.root.join("sync")
    }

    /// Stable readiness sentinel directory for `mock-serve`.
    pub(crate) fn mock_artifacts(&self) -> PathBuf {
        self.root.join("mock")
    }
}

/// Piners run artifacts beside the durable corpus and lint databases.
#[derive(Debug, Clone)]
pub(crate) struct PinersLayout {
    root: PathBuf,
}

impl PinersLayout {
    fn new(state_root: &Path) -> Self {
        Self {
            root: state_root.join("piners"),
        }
    }

    pub(crate) fn corpus_root(&self) -> PathBuf {
        self.root.join("corpus")
    }

    pub(crate) fn corpus_runs_db(&self) -> PathBuf {
        self.corpus_root().join("runs.db")
    }
}

/// Dellingr harness output, separate from configurable data scratch so a
/// project without a data directory does not gain one just for brokkr files.
#[derive(Debug, Clone)]
pub(crate) struct DellingrLayout {
    scratch: PathBuf,
}

impl DellingrLayout {
    fn new(state_root: &Path) -> Self {
        Self {
            scratch: state_root.join("dellingr"),
        }
    }

    pub(crate) fn scratch(&self) -> &Path {
        &self.scratch
    }
}

/// Mogwai harness output, separate from configurable data scratch so a
/// project without a data directory does not gain one just for brokkr files.
#[derive(Debug, Clone)]
pub(crate) struct MogwaiLayout {
    scratch: PathBuf,
}

impl MogwaiLayout {
    fn new(state_root: &Path) -> Self {
        Self {
            scratch: state_root.join("mogwai"),
        }
    }

    pub(crate) fn scratch(&self) -> &Path {
        &self.scratch
    }
}

#[derive(Debug, Clone)]
pub(crate) struct LitehtmlLayout {
    prepare_cache: PathBuf,
}

impl LitehtmlLayout {
    fn new(state_root: &Path) -> Self {
        Self {
            prepare_cache: state_root.join("prepare-cache"),
        }
    }

    pub(crate) fn prepare_cache(&self) -> &Path {
        &self.prepare_cache
    }
}

/// Elivagar's designated scratch and durable-output locations.
pub(crate) struct ElivagarLayout<'a> {
    paths: &'a config::ResolvedPaths,
}

impl<'a> ElivagarLayout<'a> {
    pub(crate) fn new(paths: &'a config::ResolvedPaths) -> Self {
        Self { paths }
    }

    pub(crate) fn scratch(&self) -> &Path {
        &self.paths.scratch_dir
    }

    pub(crate) fn data_dir(&self) -> &Path {
        &self.paths.data_dir
    }

    pub(crate) fn tilegen_output(&self) -> PathBuf {
        self.scratch().join("bench-self-output.pmtiles")
    }

    pub(crate) fn tilegen_tmp(&self) -> PathBuf {
        // The `--tmp-dir` argument passed to elivagar, under the data dir.
        Self::tilegen_tmp_in(&self.paths.data_dir)
    }

    pub(crate) fn tilegen_tmp_in(data_dir: &Path) -> PathBuf {
        data_dir.join("tilegen_tmp")
    }

    pub(crate) fn corpus_calibrands(&self) -> PathBuf {
        Self::corpus_calibrands_in(&self.paths.data_dir)
    }

    pub(crate) fn corpus_calibrands_in(data_dir: &Path) -> PathBuf {
        data_dir.join(super::CORPUS_CALIBRAND_DIR)
    }

    pub(crate) fn output(&self) -> &Path {
        &self.paths.output_dir
    }

    pub(crate) fn planetiler_output(&self) -> PathBuf {
        self.scratch().join("planetiler-bench.pmtiles")
    }

    pub(crate) fn planetiler_tmp(&self) -> PathBuf {
        self.scratch().join("planetiler_tmp")
    }

    pub(crate) fn planetiler_sources(&self) -> PathBuf {
        self.paths.data_dir.join("sources")
    }

    pub(crate) fn planetiler_primed_marker(&self) -> PathBuf {
        self.planetiler_sources().join(".brokkr-primed")
    }

    pub(crate) fn tilemaker_output(&self) -> PathBuf {
        self.scratch().join("tilemaker-bench.pmtiles")
    }

    pub(crate) fn datasets(&self) -> &HashMap<String, config::Dataset> {
        &self.paths.datasets
    }
}

/// Nidhogg's mode-specific ingest output directories.
pub(crate) struct NidhoggLayout<'a> {
    paths: &'a config::ResolvedPaths,
}

impl<'a> NidhoggLayout<'a> {
    pub(crate) fn new(paths: &'a config::ResolvedPaths) -> Self {
        Self { paths }
    }

    pub(crate) fn run_ingest_output(&self) -> PathBuf {
        self.paths.scratch_dir.join("run-ingest-output")
    }

    pub(crate) fn bench_ingest_output(&self) -> PathBuf {
        self.paths.scratch_dir.join("bench-ingest-output")
    }

    pub(crate) fn hotpath_ingest_output(&self) -> PathBuf {
        self.paths.scratch_dir.join("hotpath-ingest-output")
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum PbfhoggFileKind {
    Pbf,
    Osc,
}

/// Pbfhogg's known scratch outputs. These names come from the command layer;
/// arbitrary `.pbf`/`.osc.gz` files in scratch are not targets.
pub(crate) struct PbfhoggLayout<'a> {
    paths: &'a config::ResolvedPaths,
}

impl<'a> PbfhoggLayout<'a> {
    pub(crate) fn new(paths: &'a config::ResolvedPaths) -> Self {
        Self { paths }
    }

    pub(crate) fn verify_output(&self) -> PathBuf {
        self.paths.target_dir.join(crate::target_namespace::VERIFY)
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
        };
        scratch_dir.join(format!("{prefix}-{command}-output.{extension}"))
    }

    pub(crate) fn multi_extract_dir(scratch_dir: &Path) -> PathBuf {
        scratch_dir.join("multi-extract")
    }

    pub(crate) fn fixed_scratch_dir(scratch_dir: &Path, name: &str) -> PathBuf {
        scratch_dir.join(name)
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

        let scratch_dir = self.scratch();
        let mut files = BTreeSet::new();
        // Pbfhogg's current writers do not create a `.pbfhogg-external-join-*`
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
        }

        self.add_merged_pbf_caches(&mut files);
        files.into_iter().collect()
    }

    pub(crate) fn scratch_dirs(&self) -> Vec<PathBuf> {
        let scratch_dir = self.scratch();
        let mut dirs = BTreeSet::new();
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

/// Sidecar marker pipes under a project's configured scratch directory.
/// The reserved filename envelope plus FIFO type identifies the path; clean
/// never extracts a process id from the name.
pub(crate) struct SidecarFifos<'a> {
    scratch: &'a Path,
}

impl<'a> SidecarFifos<'a> {
    pub(crate) fn new(scratch: &'a Path) -> Self {
        Self { scratch }
    }

    pub(crate) fn existing(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(self.scratch) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| {
                if !entry.file_type().ok()?.is_fifo() {
                    return None;
                }
                let name = entry.file_name();
                let name = name.to_str()?;
                let token = name.strip_prefix(".sidecar-")?.strip_suffix(".fifo")?;
                (!token.is_empty()).then(|| entry.path())
            })
            .collect()
    }
}

/// A name-prefix namespace under Cargo's target directory. Old hashes cannot
/// be reconstructed after a sweep changes, so the reserved namespace is the
/// identity of these isolated targets.
pub(crate) struct RustflagsTargets<'a> {
    target_root: &'a Path,
}

impl<'a> RustflagsTargets<'a> {
    pub(crate) fn new(target_root: &'a Path) -> Self {
        Self { target_root }
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
                    .starts_with(RUSTFLAGS_TARGET_PREFIX);
                (is_reserved && entry.file_type().ok()?.is_dir()).then(|| entry.path())
            })
            .collect()
    }
}
