use std::path::Path;

use crate::config;
use crate::project::Project;

/// Common parameters for benchmark command handlers.
pub(crate) struct BenchRequest<'a> {
    pub(crate) dev_config: &'a config::DevConfig,
    pub(crate) project: Project,
    pub(crate) project_root: &'a Path,
    pub(crate) build_root: Option<&'a Path>,
    pub(crate) dataset: &'a str,
    pub(crate) pbf: Option<&'a str>,
    pub(crate) runs: usize,
    pub(crate) features: &'a [String],
}

/// Common parameters for hotpath command handlers.
pub(crate) struct HotpathRequest<'a> {
    pub(crate) dev_config: &'a config::DevConfig,
    pub(crate) project: Project,
    pub(crate) project_root: &'a Path,
    pub(crate) build_root: Option<&'a Path>,
    pub(crate) dataset: &'a str,
    pub(crate) pbf: Option<&'a str>,
    pub(crate) runs: usize,
    pub(crate) all_features: &'a [&'a str],
    pub(crate) alloc: bool,
    pub(crate) no_mem_check: bool,
}

/// Common parameters for profile command handlers.
pub(crate) struct ProfileRequest<'a> {
    pub(crate) dev_config: &'a config::DevConfig,
    pub(crate) project: Project,
    pub(crate) project_root: &'a Path,
    pub(crate) build_root: Option<&'a Path>,
    pub(crate) dataset: &'a str,
    pub(crate) pbf: Option<&'a str>,
    pub(crate) features: &'a [String],
    pub(crate) no_mem_check: bool,
}

/// Query parameters for the results command.
pub(crate) struct ResultsQuery {
    pub(crate) query: Option<String>,
    pub(crate) commit: Option<String>,
    pub(crate) compare: Option<Vec<String>>,
    pub(crate) compare_last: bool,
    pub(crate) command: Option<String>,
    pub(crate) variant: Option<String>,
    pub(crate) limit: usize,
    pub(crate) top: usize,
}
