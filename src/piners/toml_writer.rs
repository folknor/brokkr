//! Shared comment-preserving TOML writer primitives for the two pin registries.

use toml_edit::{Item, RawString, Table, Value};

use crate::error::DevError;
use crate::piners::registry::FilePin;

/// Normalize a block's leading whitespace without disturbing comment decor.
pub(crate) fn set_block_prefix(table: &mut Table, first: bool) {
    let decor = table.decor_mut();
    let prefix = decor
        .prefix()
        .and_then(RawString::as_str)
        .unwrap_or("")
        .to_owned();
    if prefix.chars().all(char::is_whitespace) {
        decor.set_prefix(if first { "" } else { "\n" });
    }
}

/// Set `key = value`, preserving the existing value's spacing and trailing
/// comment when the key already exists.
pub(crate) fn set_value(table: &mut Table, key: &str, new: Value) {
    if let Some(Item::Value(existing)) = table.get_mut(key) {
        let mut new = new;
        *new.decor_mut() = existing.decor().clone();
        *existing = new;
    } else {
        table.insert(key, Item::Value(new));
    }
}

/// Set an optional field, removing its key when the value is absent.
pub(crate) fn sync_opt(table: &mut Table, key: &str, value: Option<Value>) {
    match value {
        Some(value) => set_value(table, key, value),
        None => {
            table.remove(key);
        }
    }
}

/// Sort fields by their declared rank; unknown keys stay after known keys.
pub(crate) fn sort_fields(table: &mut Table, order: &[&str]) {
    table.sort_values_by(|k1, _, k2, _| rank(order, k1.get()).cmp(&rank(order, k2.get())));
}

fn rank(order: &[&str], key: &str) -> usize {
    order
        .iter()
        .position(|field| *field == key)
        .unwrap_or(order.len())
}

/// Format one shared `FilePin` value in the registries' inline-table style.
pub(crate) fn file_pin(pin: &FilePin, label: &str) -> Result<Value, DevError> {
    parse_value(
        &format!(
            "{{ path = {}, xxh128 = {} }}",
            toml_string(&pin.path.to_string_lossy()),
            toml_string(&pin.xxh128),
        ),
        label,
    )
}

/// Parse a TOML value formatted by the writer; failure is a writer bug.
pub(crate) fn parse_value(text: &str, label: &str) -> Result<Value, DevError> {
    text.parse().map_err(|e| {
        DevError::Config(format!(
            "{label} writer produced an invalid TOML value `{text}`: {e}"
        ))
    })
}

/// Serialize a TOML string value through `toml_edit`'s basic-string writer.
pub(crate) fn toml_string(s: &str) -> String {
    Value::from(s).to_string()
}
