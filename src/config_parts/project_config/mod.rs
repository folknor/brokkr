pub mod dellingr;
pub mod elivagar;
pub mod litehtml;
pub mod mogwai;
pub mod piners;
pub mod ratatoskr;
pub mod sluggrs;

use crate::error::DevError;
use crate::project::Project;

#[derive(Debug, Clone)]
pub enum ProjectConfig {
    Litehtml(litehtml::LitehtmlConfig),
    Sluggrs(sluggrs::SluggrsConfig),
    Ratatoskr(ratatoskr::RatatoskrConfig),
    Piners(piners::PinersConfig),
    Dellingr(dellingr::DellingrConfig),
    Mogwai(mogwai::MogwaiConfig),
    Generic,
}

impl ProjectConfig {
    /// Parse only the section owned by this project and reject any known
    /// project section attached to a different project identity.
    pub(crate) fn parse(project: Project, table: &toml::Table) -> Result<Self, DevError> {
        reject_foreign_sections(project, table)?;
        match project {
            Project::Litehtml => Ok(Self::Litehtml(litehtml::parse(table)?)),
            Project::Sluggrs => Ok(Self::Sluggrs(sluggrs::parse(table)?)),
            Project::Ratatoskr => Ok(Self::Ratatoskr(ratatoskr::parse(table)?)),
            // Piners keeps its documented path defaults when the table is
            // absent; the other project handlers require their section.
            Project::Piners => Ok(Self::Piners(piners::parse(table)?.unwrap_or_default())),
            Project::Dellingr => Ok(Self::Dellingr(dellingr::parse(table)?)),
            Project::Mogwai => Ok(Self::Mogwai(mogwai::parse(table)?)),
            _ => Ok(Self::Generic),
        }
    }

    /// The `[ratatoskr]` section. Load requires it in a ratatoskr project,
    /// so the refusal here only fires for a command run from another project.
    pub(crate) fn ratatoskr(&self) -> Result<&ratatoskr::RatatoskrConfig, DevError> {
        match self {
            Self::Ratatoskr(cfg) => Ok(cfg),
            _ => Err(DevError::Config(
                "this command needs a ratatoskr project (`project = \"ratatoskr\"`)".into(),
            )),
        }
    }

    /// The `[piners]` section, defaulted at load when the table is absent.
    pub(crate) fn piners(&self) -> Result<&piners::PinersConfig, DevError> {
        match self {
            Self::Piners(cfg) => Ok(cfg),
            _ => Err(DevError::Config(
                "this command needs a piners project (`project = \"piners\"`)".into(),
            )),
        }
    }
}

fn reject_foreign_sections(project: Project, table: &toml::Table) -> Result<(), DevError> {
    for (section, owner) in [
        ("litehtml", Project::Litehtml),
        ("sluggrs", Project::Sluggrs),
        ("ratatoskr", Project::Ratatoskr),
        ("piners", Project::Piners),
        ("dellingr", Project::Dellingr),
        ("mogwai", Project::Mogwai),
    ] {
        if table.contains_key(section) && project != owner {
            return Err(DevError::Config(format!(
                "[{section}] belongs to project \"{}\" (current project: \"{}\")",
                owner.name(),
                project.name(),
            )));
        }
    }
    Ok(())
}
