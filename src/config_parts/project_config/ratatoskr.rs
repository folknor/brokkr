//! `[ratatoskr]`, retired with `brokkr service`, `sync` and `mock-serve`:
//! ratatoskr runs its harness, sync benches and gates through its own
//! in-tree tools. The table still loads, whatever it holds, subtables
//! included, and stays in the composed config, so `config()` and `from`
//! paths see it as before; brokkr no longer validates or requires it.

/// Warn when the ratatoskr project's config still carries `[ratatoskr]`.
pub(crate) fn note_retired(table: &toml::Table) {
    if table.contains_key("ratatoskr") {
        crate::output::warn(
            "[ratatoskr] is retired with brokkr service, sync and mock-serve: brokkr no longer \
             validates or requires it, scripts still see it; it can be removed",
        );
    }
}
