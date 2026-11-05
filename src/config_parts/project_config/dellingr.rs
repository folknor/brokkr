//! `[dellingr]`, retired with the `brokkr dellingr` driver: dellingr
//! benchmarks through script benches (`[bench.*]`). The table still loads,
//! whatever it holds, and stays in the composed config, so `config()` and
//! `from` paths see it as before; brokkr no longer validates it.

/// Warn when the dellingr project's config still carries `[dellingr]`.
pub(crate) fn note_retired(table: &toml::Table) {
    if table.contains_key("dellingr") {
        crate::output::warn(
            "[dellingr] is retired with the brokkr dellingr driver: brokkr no longer validates \
             it, scripts still see it; dellingr benchmarks through [bench.*] (brokkr man bench)",
        );
    }
}
