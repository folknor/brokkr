#[path = "config_parts/check.rs"]
pub mod check;
#[path = "config_parts/project_config/mod.rs"]
pub mod project_config;

include!("config_parts/schema.rs");
include!("config_parts/parser.rs");
include!("config_parts/compose.rs");
include!("config_parts/user.rs");
include!("config_parts/tests.rs");

/// Load a project config while retaining the composition provenance for
/// callers that need to report or apply file-level policy without composing
/// the config a second time.
pub fn load_with_sources(
    project_root: &std::path::Path,
) -> Result<
    (
        crate::project::Project,
        DevConfig,
        project_config::ProjectConfig,
        ConfigSources,
    ),
    crate::error::DevError,
> {
    let (root, sources) = compose(&project_root.join("brokkr.toml"))?;
    let project = parse_project(&root)?;
    let project_config = project_config::ProjectConfig::parse(&project, &root)?;
    let mut config = parse_dev_config(&root)?;
    config.included_config_files = sources.included().to_vec();
    Ok((project, config, project_config, sources))
}
