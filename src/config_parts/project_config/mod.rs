//! `ProjectConfig`: the one project-owned section a config may hold.
//!
//! Load rejects a known project section when `project` names another
//! project, and the litehtml-rs, sluggrs and mogwai parsers require their
//! section. `[dellingr]`, `[piners]` and `[ratatoskr]` are retired with their
//! commands: a matching project gets a warning, and the table loads
//! unvalidated and stays in the composed config a script sees. `[pbfhogg]` is
//! an ordinary host-name section, not a project section. `Detection` and
//! `MeasureRequest` hold the parsed variant, so handlers match on it instead
//! of re-checking presence.

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
            Project::Ratatoskr => {
                self::ratatoskr::note_retired(table);
                Ok(Self::Generic)
            }
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
