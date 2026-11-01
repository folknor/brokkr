//! pbfhogg's `read`, `write` and `merge` benchmarks: one measured cell per
//! variant, run as a batch on one build.
//!
//! A cell is a read mode (`bench-read --mode`), a writer and compression
//! (`bench-write`), or an I/O mode and compression (`bench-merge`). A batch
//! resolves its cells before anything is built, so `--dry-run` prints every
//! cell's argv; it then builds once (the measured mode's instrumentation
//! feature included, and `linux-io-uring` for a merge batch with `--uring`,
//! which every cell of that batch runs on), opens one pbfhogg handle, and
//! materialises each cell's invocation under it. Each cell is measured
//! through [`execute_cell`], the one place a measured invocation's mode is
//! decided, so `--hotpath` and `--alloc` profile these children as they do
//! every other command's.
//!
//! The children time their own measured region and print it, so a cell's row
//! records the self-reported wall in every mode, profiled ones included.
//!
//! One brokkr invocation runs several cells, so the brokkr argv cannot tell
//! their rows apart; each cell's row carries its discriminators as its
//! `arm_qualifier` ([`BenchCell::qualifier`]), the convention extract uses
//! for a strategy fan-out. Rows stored before cells were qualified are
//! qualified by the results.db migration from their recorded `cli_args`, by
//! [`historical_qualifier`], which only recognises the exact shapes these
//! benchmarks have emitted.

use std::path::{Path, PathBuf};

use crate::clean::layout::PbfhoggLayout;
use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness, WallSource};
use crate::measure::MeasureMode;
use crate::measure::runner::{Cell, CellHooks, execute_cell};
use crate::output::child::Exits;
use crate::pbfhogg::argv::PbfArgv;
use crate::pbfhogg::binary::PbfhoggBinary;
use crate::reservations::PRIVATE_DIR_PLACEHOLDER;

/// `bench-read` modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadMode {
    Sequential,
    Parallel,
    Pipelined,
    BlobReader,
}

impl ReadMode {
    pub fn name(self) -> &'static str {
        match self {
            ReadMode::Sequential => "sequential",
            ReadMode::Parallel => "parallel",
            ReadMode::Pipelined => "pipelined",
            ReadMode::BlobReader => "blobreader",
        }
    }
}

pub const ALL_READ_MODES: &[ReadMode] = &[
    ReadMode::Sequential,
    ReadMode::Parallel,
    ReadMode::Pipelined,
    ReadMode::BlobReader,
];

/// Parse comma-separated read mode names.
pub fn parse_read_modes(input: &str) -> Result<Vec<ReadMode>, DevError> {
    let mut modes = Vec::new();
    for token in input.split(',') {
        let trimmed = token.trim();
        let mode = match trimmed.to_ascii_lowercase().as_str() {
            "sequential" => ReadMode::Sequential,
            "parallel" => ReadMode::Parallel,
            "pipelined" => ReadMode::Pipelined,
            "blobreader" => ReadMode::BlobReader,
            _ => return Err(DevError::Config(format!("unknown read mode: {trimmed}"))),
        };
        modes.push(mode);
    }
    Ok(modes)
}

/// What one cell runs.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CellKind {
    Read { mode: ReadMode },
    Write { writer: &'static str, compression: String },
    Merge { io: &'static str, compression: String },
}

/// One measured variant of a batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BenchCell {
    kind: CellKind,
    /// The console label, as these benchmarks have always printed it.
    name: String,
}

impl BenchCell {
    /// The row's `command`.
    pub(crate) fn command(&self) -> &'static str {
        match self.kind {
            CellKind::Read { .. } => "read",
            CellKind::Write { .. } => "write",
            CellKind::Merge { .. } => "merge",
        }
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// The row's `arm_qualifier`: the discriminators that tell this cell
    /// from the batch's others, `key=value` pairs joined by `;` in a fixed
    /// order. Every value comes from a closed set or the compression grammar,
    /// neither of which can hold `;` or `=`.
    pub(crate) fn qualifier(&self) -> String {
        match &self.kind {
            CellKind::Read { mode } => read_qualifier(mode.name()),
            CellKind::Write {
                writer,
                compression,
            } => write_qualifier(writer, compression),
            CellKind::Merge { io, compression } => merge_qualifier(io, compression),
        }
    }

    /// The cell's pbfhogg argv, with what it reads and writes.
    pub(crate) fn argv(&self, inputs: &BatchInputs<'_>) -> Result<PbfArgv, DevError> {
        match &self.kind {
            CellKind::Read { mode } => Ok(PbfArgv::new("bench-read")
                .input(inputs.pbf)?
                .arg("--mode")
                .arg(mode.name())),
            // `bench-write` encodes in memory and names no output path.
            CellKind::Write {
                writer,
                compression,
            } => Ok(PbfArgv::new("bench-write")
                .input(inputs.pbf)?
                .arg("--compression")
                .arg(compression.clone())
                .arg("--writer")
                .arg(*writer)),
            // Every merge cell and iteration writes the same output file.
            CellKind::Merge { io, compression } => {
                let osc = inputs.osc.ok_or_else(|| {
                    DevError::Internal("a merge cell needs the batch's OSC input".into())
                })?;
                PbfArgv::new("bench-merge")
                    .input(inputs.pbf)?
                    .input(osc)?
                    .output(&inputs.merge_output)
                    .map(|argv| {
                        argv.arg("--compression")
                            .arg(compression.clone())
                            .arg("--io-mode")
                            .arg(*io)
                    })
            }
        }
    }
}

fn read_qualifier(mode: &str) -> String {
    format!("mode={mode}")
}

fn write_qualifier(writer: &str, compression: &str) -> String {
    format!("writer={writer};compression={compression}")
}

fn merge_qualifier(io: &str, compression: &str) -> String {
    format!("io={io};compression={compression}")
}

/// One cell per read mode.
pub(crate) fn read_cells(modes: &[ReadMode]) -> Vec<BenchCell> {
    modes
        .iter()
        .map(|&mode| BenchCell {
            kind: CellKind::Read { mode },
            name: mode.name().to_owned(),
        })
        .collect()
}

/// A sync and a pipelined cell per compression.
pub(crate) fn write_cells(compressions: &[String]) -> Vec<BenchCell> {
    compressions
        .iter()
        .flat_map(|compression| {
            ["sync", "pipelined"].map(|writer| BenchCell {
                kind: CellKind::Write {
                    writer,
                    compression: compression.clone(),
                },
                name: format!("{writer}-{compression}"),
            })
        })
        .collect()
}

/// A buffered cell per compression, and a uring one beside it with `uring`.
pub(crate) fn merge_cells(compressions: &[String], uring: bool) -> Vec<BenchCell> {
    let io_modes: &[&'static str] = if uring {
        &["buffered", "uring"]
    } else {
        &["buffered"]
    };
    compressions
        .iter()
        .flat_map(|compression| {
            io_modes.iter().map(move |&io| BenchCell {
                kind: CellKind::Merge {
                    io,
                    compression: compression.clone(),
                },
                name: format!("{io}+{compression}"),
            })
        })
        .collect()
}

/// What a batch's cells read and write.
pub(crate) struct BatchInputs<'a> {
    pub pbf: &'a Path,
    /// The OSC a merge cell applies; `None` for a batch without merge cells.
    pub osc: Option<&'a Path>,
    /// The one output every merge cell writes.
    pub merge_output: PathBuf,
}

impl<'a> BatchInputs<'a> {
    pub(crate) fn new(pbf: &'a Path, osc: Option<&'a Path>, scratch_dir: &Path) -> Self {
        Self {
            pbf,
            osc,
            merge_output: PbfhoggLayout::fixed_scratch_file(
                scratch_dir,
                "bench-merge-output.osm.pbf",
            ),
        }
    }
}

/// How one batch runs: its mode, rows and working directory.
pub(crate) struct Batch<'a> {
    pub mode: &'a MeasureMode,
    pub harness: &'a BenchHarness,
    /// The batch's one handle, which every cell's invocation resolves through.
    pub handle: &'a PbfhoggBinary,
    /// The variant label the console names each cell under.
    pub label: &'a str,
    pub file_mb: f64,
    pub runs: usize,
    pub cwd: &'a Path,
}

/// Measure every cell, each independently: a failed cell, its invocation's
/// negotiation included, does not skip the rest. Removing the merge output is
/// attempted after the batch on every outcome, and a removal that fails is
/// reported.
pub(crate) fn run_batch(
    batch: &Batch<'_>,
    cells: &[BenchCell],
    inputs: &BatchInputs<'_>,
) -> Result<(), DevError> {
    let merges = cells
        .iter()
        .any(|cell| matches!(cell.kind, CellKind::Merge { .. }));
    if merges && let Some(parent) = inputs.merge_output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let basename = crate::utf8_path::file_name(inputs.pbf);
    let result = crate::bench_variants::run_variants_of(batch.label, cells, BenchCell::name, |cell| {
        let invocation = batch.handle.invocation(cell.argv(inputs)?)?;
        let measured = Cell {
            config: BenchConfig {
                command: cell.command().into(),
                input_file: Some(basename.clone()),
                input_mb: Some(batch.file_mb),
                cargo_features: None,
                external_build: None,
                runs: batch.runs,
                cli_args: None,
                metadata: vec![],
                arm_qualifier: Some(cell.qualifier()),
            },
            invocation: &invocation,
            exits: Exits::SUCCESS,
            wall: WallSource::SelfReported,
        };
        execute_cell(
            batch.mode,
            batch.harness,
            batch.cwd,
            &measured,
            &CellHooks::NONE,
        )
    });
    if merges {
        match crate::reservations::remove_file(&inputs.merge_output) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            // Left in scratch, and said so; the batch's own outcome stands.
            Err(error) => crate::output::warn(&format!(
                "merge output {} not removed: {error}",
                crate::output::path(&inputs.merge_output)
            )),
        }
    }
    result
}

/// The qualifier a row stored before cells were qualified would carry, from
/// its recorded `cli_args`, or `None` where that string does not prove it.
///
/// `cli_args` is a display string: the program, then each argument, joined
/// by single spaces, an argument holding a space wrapped in double quotes
/// with nothing escaped. So a row whose `cli_args` holds a quote is not
/// recognised, and every other one splits on spaces into exactly the tokens
/// that ran. Only the shapes these benchmarks emitted are accepted, the whole
/// argv after the program, no flag searched for anywhere in it:
///
/// - `bench-read IN --mode M`
/// - `bench-write IN --compression C --writer W`
/// - `bench-merge BASE OSC (-o|--output) OUT --compression C --io-mode I`,
///   then the flags the handle negotiated, in the order it appends them:
///   `--overwrite`, then `--staging-dir D`, then `--tmp-dir D`, each at most
///   once, `D` being the private-directory placeholder or, from builds before
///   per-launch reservations, `OUT`'s directory joined with `.brokkr-pbfhogg`.
///
/// A value is taken as recorded, a retired one (read mode `mmap`, merge I/O
/// `uring-sqpoll`) included, as long as it is a plain token that cannot hold
/// a delimiter.
pub(crate) fn historical_qualifier(command: &str, cli_args: &str) -> Option<String> {
    if cli_args.contains('"') {
        return None;
    }
    let tokens: Vec<&str> = cli_args.split(' ').collect();
    let (program, argv) = tokens.split_first()?;
    if program.is_empty() {
        return None;
    }
    match (command, argv) {
        ("read", ["bench-read", input, "--mode", mode]) if is_path(input) => {
            Some(read_qualifier(value(mode)?))
        }
        (
            "write",
            [
                "bench-write",
                input,
                "--compression",
                compression,
                "--writer",
                writer,
            ],
        ) if is_path(input) => Some(write_qualifier(value(writer)?, value(compression)?)),
        ("merge", ["bench-merge", base, osc, output_flag, output, rest @ ..])
            if is_path(base)
                && is_path(osc)
                && is_path(output)
                && matches!(*output_flag, "-o" | "--output") =>
        {
            let ["--compression", compression, "--io-mode", io, tail @ ..] = rest else {
                return None;
            };
            negotiated_tail(tail, output)?;
            Some(merge_qualifier(value(io)?, value(compression)?))
        }
        _ => None,
    }
}

/// A recorded value that cannot hold a qualifier delimiter or be a flag.
fn value(token: &str) -> Option<&str> {
    let plain = !token.is_empty()
        && !token.starts_with('-')
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '-' | '_' | '.'));
    plain.then_some(token)
}

/// A positional path: present, and not a flag.
fn is_path(token: &str) -> bool {
    !token.is_empty() && !token.starts_with('-')
}

/// Exactly the flags a pbfhogg handle could have appended to a merge cell.
fn negotiated_tail(tail: &[&str], output: &str) -> Option<()> {
    let fixed_dir = match output.rsplit_once('/') {
        Some((parent, _)) if !parent.is_empty() => format!("{parent}/.brokkr-pbfhogg"),
        Some(_) => "/.brokkr-pbfhogg".to_owned(),
        None => ".brokkr-pbfhogg".to_owned(),
    };
    let private = |dir: &str| dir == PRIVATE_DIR_PLACEHOLDER || dir == fixed_dir;
    let mut rest = tail;
    if let ["--overwrite", after @ ..] = rest {
        rest = after;
    }
    if let ["--staging-dir", dir, after @ ..] = rest {
        if !private(dir) {
            return None;
        }
        rest = after;
    }
    if let ["--tmp-dir", dir, after @ ..] = rest {
        if !private(dir) {
            return None;
        }
        rest = after;
    }
    rest.is_empty().then_some(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn inputs() -> BatchInputs<'static> {
        BatchInputs::new(
            Path::new("/data/denmark.osm.pbf"),
            Some(Path::new("/data/denmark-4705.osc.gz")),
            Path::new("/data/scratch"),
        )
    }

    /// The qualifier a new row stores is exactly the one the recognizer
    /// gives a stored row of the same cell, so the two populations pair.
    #[test]
    fn a_cell_and_its_recorded_argv_give_the_same_qualifier() {
        let mut cells = read_cells(ALL_READ_MODES);
        cells.extend(write_cells(&["none".into(), "zstd:3".into()]));
        cells.extend(merge_cells(&["zlib".into(), "none".into()], true));
        for cell in &cells {
            let argv = cell.argv(&inputs()).unwrap();
            let cli_args = format!("/target/release/pbfhogg {}", argv.args().join(" "));
            assert_eq!(
                historical_qualifier(cell.command(), &cli_args),
                Some(cell.qualifier()),
                "{cli_args}"
            );
        }
        // Every cell of a batch is told apart.
        let qualifiers: std::collections::BTreeSet<(&str, String)> = cells
            .iter()
            .map(|cell| (cell.command(), cell.qualifier()))
            .collect();
        assert_eq!(qualifiers.len(), cells.len());
    }

    #[test]
    fn the_shapes_actually_stored_are_recognised() {
        assert_eq!(
            historical_qualifier("read", "/p/pbfhogg bench-read /d/planet.osm.pbf --mode mmap")
                .as_deref(),
            Some("mode=mmap")
        );
        assert_eq!(
            historical_qualifier(
                "write",
                "/p/pbfhogg bench-write /d/dk.osm.pbf --compression zlib:6 --writer pipelined"
            )
            .as_deref(),
            Some("writer=pipelined;compression=zlib:6")
        );
        assert_eq!(
            historical_qualifier(
                "merge",
                "/p/pbfhogg bench-merge /d/dk.osm.pbf /d/dk.osc.gz -o /s/out.osm.pbf \
                 --compression zlib --io-mode uring-sqpoll"
            )
            .as_deref(),
            Some("io=uring-sqpoll;compression=zlib")
        );
    }

    /// The negotiated tail, in either era, and nothing else after the cell's
    /// own flags.
    #[test]
    fn a_merge_tail_is_only_what_the_handle_appended() {
        let base = "/p/pbfhogg bench-merge /d/b.osm.pbf /d/c.osc.gz --output /s/out.osm.pbf \
                    --compression none --io-mode buffered";
        for tail in [
            "",
            " --overwrite",
            " --overwrite --staging-dir @brokkr-private-dir@",
            " --staging-dir @brokkr-private-dir@",
            " --overwrite --staging-dir /s/.brokkr-pbfhogg",
            " --overwrite --staging-dir /s/.brokkr-pbfhogg --tmp-dir /s/.brokkr-pbfhogg",
        ] {
            assert_eq!(
                historical_qualifier("merge", &format!("{base}{tail}")).as_deref(),
                Some("io=buffered;compression=none"),
                "{tail}"
            );
        }
        for tail in [
            " --staging-dir /elsewhere/.brokkr-pbfhogg",
            " --staging-dir @brokkr-private-dir@ --overwrite",
            " --overwrite --overwrite",
            " --direct-io",
            " extra",
        ] {
            assert_eq!(historical_qualifier("merge", &format!("{base}{tail}")), None, "{tail}");
        }
    }

    #[test]
    fn anything_unproven_is_left_alone() {
        for (command, cli_args) in [
            // A quoted argument: its boundaries are not recoverable.
            ("read", "/p/pbfhogg bench-read \"/d/my file.pbf\" --mode parallel"),
            // The wrong command for the subcommand, or a different subcommand.
            ("write", "/p/pbfhogg bench-read /d/x.pbf --mode parallel"),
            ("read", "/p/pbfhogg check --refs /d/x.pbf"),
            // A flag searched for anywhere would match these.
            ("read", "/p/pbfhogg bench-read /d/x.pbf --mode parallel --direct-io"),
            ("read", "/p/pbfhogg bench-read --mode parallel /d/x.pbf"),
            // A value that could hold a delimiter, or is a flag.
            ("read", "/p/pbfhogg bench-read /d/x.pbf --mode a;b"),
            ("read", "/p/pbfhogg bench-read /d/x.pbf --mode --writer"),
            ("write", "/p/pbfhogg bench-write /d/x.pbf --writer sync --compression none"),
            ("read", ""),
        ] {
            assert_eq!(historical_qualifier(command, cli_args), None, "{cli_args}");
        }
    }
}
