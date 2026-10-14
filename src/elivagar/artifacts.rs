use crate::clean::layout::ElivagarLayout;
use crate::elivagar::commands::BuiltCommand;
use crate::error::DevError;
use crate::output;

/// After a successful tilegen run, move the raw output PMTiles from the
/// scratch dir to the durable output store using the archive key
/// and print the path. The output dir is deliberately NOT the scratch/tmp dir:
/// elivagar wipes its `--tmp-dir` at every run start, so an archive written
/// into scratch would be destroyed by the next run (which is exactly how a
/// probe pair was already lost). The store owns the retention window for each
/// dataset and variant.
///
/// Example commands have no output files to promote.
///
/// Every way of ending without a durable archive is an `Err`, not a logged
/// line: a tilegen run whose output never reaches the store has not produced
/// what `pmtiles-inspect`/`regress`/`pmtiles-corpus` will go looking for, and
/// returning success there let a caller (or a script) move on to a resolver
/// that then reports "no build" with nothing tying it to this run. The scratch
/// output is left in place on every failure path so it can be moved by hand.
pub(crate) fn rename_elivagar_output(
    command: &BuiltCommand<'_>,
    layout: &ElivagarLayout<'_>,
    dataset: &str,
    variant: &str,
    git_root: &std::path::Path,
) -> Result<(), DevError> {
    let output_files = command.output_files(layout);
    if output_files.is_empty() {
        return Ok(());
    }

    // Provenance must name the commit whose code produced these tiles. For a
    // `--commit <hash>` run that is the worktree's HEAD (`git_root`), NOT the
    // main tree's HEAD - collecting from the main tree would stamp an
    // unrelated commit onto an old build's output (and feed the same
    // misattribution as a stale binary). Callers pass the request's build root.
    //
    // A git failure is fatal rather than a `-unknown` suffix: no resolver
    // constructs that name, so the archive would sit in the store unreachable
    // by every consumer while the run reported success.
    let commit = crate::git::collect(git_root)
        .map_err(|e| {
            DevError::Io(std::io::Error::other(format!(
                "cannot name the tilegen archive: git commit of {} unavailable ({e}); \
                 output left in {}",
                git_root.display(),
                layout.scratch().display(),
            )))
        })?
        .commit
        .as_str()
        .to_owned();

    let store = super::archive_store::ArchiveStore::new(layout.output());
    let key = super::archive_store::ArchiveKey::new(dataset, variant, commit)?;
    let tmp_dir = layout.tilegen_tmp();
    for path in &output_files {
        let dest = store.admit(path, &key, &[layout.scratch(), &tmp_dir])?;
        output::run_msg(&format!("output: {}", dest.display()));
    }
    Ok(())
}
