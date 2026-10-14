//! PMTiles archive statistics.
use crate::{cli, error::DevError, output, pmtiles, project::Project};

pub(crate) fn cmd_pmtiles_stats(project: Project, files: &[String]) -> Result<(), DevError> {
    // Dispatch applies this same catalogue entry; keeping the direct-entry
    // guard here reuses that policy rather than defining a second project map.
    cli::CommandId::PmtilesStats.spec().require(project)?;

    // Every file is attempted - one bad archive does not hide the stats of the
    // rest - but any failure fails the command, as `pmtiles::run` now reports
    // an unreadable file as an error rather than printing it as output.
    let mut failed = 0usize;
    for file in files {
        if let Err(e) = pmtiles::run(file) {
            output::error(&e.to_string());
            failed += 1;
        }
    }
    if failed > 0 {
        return Err(DevError::Reported(format!(
            "{failed} of {} could not be read",
            output::count(files.len(), "PMTiles file")
        )));
    }
    Ok(())
}

#[cfg(test)]
mod pmtiles_stats_gate_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    // The gate must agree with `visibility.rs`'s catalogue entry, which scopes
    // pmtiles-stats to elivagar + nidhogg. Wrong projects get the real
    // `require`-shaped error; allowed projects fall through (empty file list
    // makes the call a pure no-op, so no disk I/O).

    #[test]
    fn rejects_projects_without_pmtiles() {
        for project in [
            Project::Pbfhogg,
            Project::Litehtml,
            Project::Piners,
            Project::Other("mystery"),
        ] {
            let err = cmd_pmtiles_stats(project, &[]).unwrap_err();
            let DevError::WrongProject { .. } = err else {
                panic!("expected DevError::WrongProject, got {err:?}");
            };
            let msg = err.to_string();
            assert!(msg.contains("pmtiles-stats"), "message: {msg}");
            assert!(msg.contains("elivagar"), "message: {msg}");
            assert!(msg.contains("nidhogg"), "message: {msg}");
        }
    }

    #[test]
    fn allows_elivagar_and_nidhogg() {
        // Empty file list => the loop body never runs, so an allowed project
        // returns Ok without opening any archive.
        assert!(cmd_pmtiles_stats(Project::Elivagar, &[]).is_ok());
        assert!(cmd_pmtiles_stats(Project::Nidhogg, &[]).is_ok());
    }
}
