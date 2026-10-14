//! Project-specific verification routing.
use crate::{
    cli::VerifyCommand,
    config, context, elivagar,
    error::DevError,
    nidhogg, pbfhogg,
    project::{self, Project},
};

pub(crate) fn cmd_verify(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &project::ProjectRoot,
    build_root: &project::BuildRoot,
    verify: VerifyCommand,
    features: &[String],
    verbose: bool,
) -> Result<(), DevError> {
    // The command catalogue admits the union of verify projects. Keep these
    // variant checks here because each nested verify operation belongs to
    // exactly one project.
    match verify {
        // ----- elivagar verify variants -----
        VerifyCommand::ElivVerify {
            dataset,
            variant,
            commit,
            file,
            geometry_stats,
            unique_payloads,
        } => {
            project::require(project, Project::Elivagar, "verify")?;
            // Never reached under a historical worktree: `run()` dispatches
            // this variant before `with_worktree`, so `build_root` is the
            // main tree and the archive resolver reads its HEAD.
            elivagar::cmd::verify(
                dev_config,
                project,
                project_root,
                build_root,
                &dataset,
                &variant,
                commit.as_deref(),
                file.as_deref(),
                features,
                geometry_stats,
                unique_payloads,
            )
        }

        // ----- nidhogg verify variants -----
        // Every verify subcommand runs under the command lock the dispatch
        // takes. `batch` and `geocode` only query an already-running server;
        // `readonly` also builds the server, stops and restarts it, and chmods
        // the index, so it re-enters that hold explicitly (and `cargo_build`
        // refuses to run without one).
        VerifyCommand::Batch { dataset } => {
            project::require(project, Project::Nidhogg, "verify batch")?;
            nidhogg::cmd::verify_batch(dev_config, project_root, build_root, &dataset)
        }
        VerifyCommand::NidGeocode { queries } => {
            project::require(project, Project::Nidhogg, "verify geocode")?;
            nidhogg::cmd::verify_geocode(dev_config, &queries)
        }
        VerifyCommand::Readonly { dataset } => {
            project::require(project, Project::Nidhogg, "verify readonly")?;
            // Re-enters the hold when a `--commit` worktree already took it.
            let _lock = context::reenter_command_lock()?;
            nidhogg::cmd::verify_readonly(dev_config, project_root, build_root, &dataset, features)
        }
        // ----- pbfhogg verify variants -----
        _ => {
            project::require(project, Project::Pbfhogg, "verify")?;
            pbfhogg::cmd::verify(
                dev_config,
                project_root,
                build_root,
                verify,
                features,
                verbose,
            )
        }
    }
}
