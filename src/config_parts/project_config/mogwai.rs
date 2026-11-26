//! `[mogwai]`, retired with the `brokkr mogwai` command: mogwai benchmarks
//! through script benches (`[bench.*]`). The table still loads, whatever it
//! holds, and stays in the composed config, so `config()` and `from` paths see
//! it as before; brokkr no longer validates it.

/// Warn when the mogwai project's config still carries `[mogwai]`.
pub(crate) fn note_retired(table: &toml::Table) {
    if table.contains_key("mogwai") {
        crate::output::warn(
            "[mogwai] is retired with the brokkr mogwai command: brokkr no longer validates \
             it, scripts still see it; mogwai benchmarks through [bench.*] (brokkr man bench)",
        );
    }
}
