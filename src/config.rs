#[path = "config_parts/check.rs"]
pub mod check;
#[path = "config_parts/project_config/mod.rs"]
pub mod project_config;

include!("config_parts/schema.rs");
include!("config_parts/parser.rs");
include!("config_parts/compose.rs");
include!("config_parts/user.rs");
include!("config_parts/tests.rs");
