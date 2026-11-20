pub mod dellingr;
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
    /// Boxed: the largest section, reached only through [`Self::ratatoskr`].
    Ratatoskr(Box<ratatoskr::RatatoskrConfig>),
    Mogwai(mogwai::MogwaiConfig),
    Generic,
}

impl ProjectConfig {
    /// Parse only the section owned by this project and reject any known
    /// project section attached to a different project identity.
    pub(crate) fn parse(project: &Project, table: &toml::Table) -> Result<Self, DevError> {
        reject_foreign_sections(project, table)?;
        match project {
            Project::Litehtml => Ok(Self::Litehtml(litehtml::parse(table)?)),
            Project::Sluggrs => Ok(Self::Sluggrs(sluggrs::parse(table)?)),
            Project::Ratatoskr => Ok(Self::Ratatoskr(Box::new(ratatoskr::parse(table)?))),
            Project::Piners => {
                self::piners::note_retired(table);
                Ok(Self::Generic)
            }
            Project::Dellingr => {
                self::dellingr::note_retired(table);
                Ok(Self::Generic)
            }
            Project::Mogwai => Ok(Self::Mogwai(mogwai::parse(table)?)),
            _ => Ok(Self::Generic),
        }
    }

    /// The `[ratatoskr]` section. Load requires it in a ratatoskr project,
    /// so the refusal here only fires for a command run from another project.
    pub(crate) fn ratatoskr(&self) -> Result<&ratatoskr::RatatoskrConfig, DevError> {
        match self {
            Self::Ratatoskr(cfg) => Ok(&**cfg),
            _ => Err(DevError::Config(
                "this command needs a ratatoskr project (`project = \"ratatoskr\"`)".into(),
            )),
        }
    }
}

fn reject_foreign_sections(project: &Project, table: &toml::Table) -> Result<(), DevError> {
    for (section, owner) in [
        ("litehtml", Project::Litehtml),
        ("sluggrs", Project::Sluggrs),
        ("ratatoskr", Project::Ratatoskr),
        ("piners", Project::Piners),
        ("dellingr", Project::Dellingr),
        ("mogwai", Project::Mogwai),
    ] {
        if table.contains_key(section) && project != &owner {
            return Err(DevError::Config(format!(
                "[{section}] belongs to project \"{}\" (current project: \"{}\")",
                owner.name(),
                project.name(),
            )));
        }
    }
    Ok(())
}
