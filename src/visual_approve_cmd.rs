//! Visual baseline approval.
use crate::{
    config::project_config::ProjectConfig, error::DevError, litehtml, project::Project, sluggrs,
};
use std::path::Path;

pub(crate) fn cmd_approve(
    project_config: &ProjectConfig,
    project: Project,
    project_root: &Path,
    build_root: &Path,
    fixture: Vec<String>,
    all: bool,
) -> Result<(), DevError> {
    if fixture.is_empty() && !all {
        return Err(DevError::Usage(
            "specify one or more fixture/snapshot IDs, or --all".into(),
        ));
    }
    if all && !fixture.is_empty() {
        return Err(DevError::Usage(
            "give fixture/snapshot IDs or --all, not both".into(),
        ));
    }

    match project_config {
        ProjectConfig::Litehtml(cfg) => {
            let ids = if all {
                cfg.fixtures.iter().map(|f| f.id.clone()).collect()
            } else {
                fixture
            };
            for id in &ids {
                litehtml::cmd::approve(project, project_root, build_root, cfg, id)?;
            }
            Ok(())
        }
        ProjectConfig::Sluggrs(cfg) => {
            let ids = if all {
                cfg.snapshots.iter().map(|s| s.id.clone()).collect()
            } else {
                fixture
            };
            for id in &ids {
                sluggrs::cmd::approve(project, project_root, build_root, cfg, id)?;
            }
            Ok(())
        }
        _ => Err(DevError::WrongProject {
            command: "approve".to_owned(),
            expected: "litehtml/sluggrs".to_owned(),
            current: project.to_string(),
        }),
    }
}
