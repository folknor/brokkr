include!("cli/schema.rs");
include!("cli/validation.rs");
include!("cli/visibility.rs");

#[cfg(test)]
mod visibility_tests {
    #![allow(clippy::unwrap_used)]
    use super::{Cli, TABLE, visible_in};
    use crate::project::Project;
    use clap::CommandFactory;

    /// Every top-level subcommand must appear in the visibility table.
    ///
    /// The table is the one place that knows which commands belong to which
    /// project, and it is maintained by hand. A new subcommand added to
    /// `Command` without a table entry would fall through to the fail-open
    /// default and stay visible everywhere - safe, but quietly wrong. This
    /// test makes the table's completeness an obligation rather than a good
    /// intention.
    #[test]
    fn table_covers_every_subcommand() {
        let cmd = Cli::command();
        let missing: Vec<String> = cmd
            .get_subcommands()
            .map(|s| s.get_name().to_owned())
            .filter(|name| !TABLE.iter().any(|(n, _)| n == name))
            .collect();
        assert!(missing.is_empty(), "subcommands absent from TABLE: {missing:?}");
    }

    /// And nothing in the table may name a subcommand that no longer exists -
    /// a stale entry hides nothing and misleads the next reader.
    #[test]
    fn table_has_no_stale_entries() {
        let cmd = Cli::command();
        let names: Vec<&str> = cmd.get_subcommands().map(clap::Command::get_name).collect();
        let stale: Vec<&str> = TABLE
            .iter()
            .map(|(n, _)| *n)
            .filter(|n| !names.contains(n))
            .collect();
        assert!(stale.is_empty(), "TABLE names no longer in Command: {stale:?}");
    }

    /// The project-agnostic commands must survive in every project, including
    /// an unrecognised one - that is the whole contract of `Visibility::Any`.
    #[test]
    fn shared_commands_visible_everywhere() {
        for project in [
            Project::Pbfhogg,
            Project::Elivagar,
            Project::Nidhogg,
            Project::Brokkr,
            Project::Litehtml,
            Project::Sluggrs,
            Project::Ratatoskr,
            Project::Piners,
            Project::Other("some-foreign-repo"),
        ] {
            for name in ["check", "test", "env", "results", "history", "clean"] {
                assert!(visible_in(name, project), "`{name}` hidden in {project}");
            }
        }
    }

    /// `Other("piners")` is a foreign repo that happens to call itself piners
    /// in its own brokkr.toml - it is NOT `Project::Piners` and must not be
    /// handed piners' project-specific commands.
    #[test]
    fn other_does_not_impersonate_a_builtin() {
        assert!(visible_in("corpus", Project::Piners));
        assert!(!visible_in("corpus", Project::Other("piners")));
    }
}
