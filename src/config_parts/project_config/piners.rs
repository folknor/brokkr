//! `[piners]`, retired with `brokkr corpus` and `brokkr corpus-results`:
//! piners runs its parity corpus through its own in-tree tool. The table
//! still loads, whatever it holds, and stays in the composed config, so
//! `config()` and `from` paths see it as before; brokkr no longer validates
//! it.

/// Warn when the piners project's config still carries `[piners]`.
pub(crate) fn note_retired(table: &toml::Table) {
    if table.contains_key("piners") {
        crate::output::warn(
            "[piners] is retired with brokkr corpus and corpus-results: brokkr no longer \
             validates it, scripts still see it; piners runs its corpus through its own tool",
        );
    }
}
