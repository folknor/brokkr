//! Where an inline script's lines are in the config file that holds it.
//!
//! An inline `script` is a TOML string, so the Lua's line 1 is wherever the
//! string starts, and escapes, a multi-line string's trimmed first newline and
//! a line-ending backslash move lines around. The composition keeps no spans,
//! so the declaring file (the one `ConfigSources` says set `script`) is read
//! again and parsed with spans, the string's literal decoded here with the
//! file line of every byte it produces, and the map is used only when this
//! decoding, toml_edit's and the script that was loaded are the same bytes: a
//! file edited since, or a literal this decoder reads differently, gives no
//! map rather than a wrong one. A Lua line is counted as dellingr's lexer
//! counts it (a CRLF or LFCR pair one break, a lone CR or LF one) and is
//! named by the file line of its first byte; a line joined by a line-ending
//! backslash is named by its first file line.

use std::path::Path;

/// Each Lua line's file line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LineMap {
    /// Lua line `k` (1-based) is file line `lines[k - 1]`.
    lines: Vec<u32>,
}

impl LineMap {
    /// The file line of Lua line `line`; `None` for 0 (unknown) or a line the
    /// script does not have.
    pub(crate) fn file_line(&self, line: u32) -> Option<u32> {
        let index = usize::try_from(line).ok()?.checked_sub(1)?;
        self.lines.get(index).copied()
    }
}

/// The map for bench `bench`'s inline script, `loaded`, from the file `path`
/// that set it.
pub(crate) fn locate(path: &Path, bench: &str, loaded: &str) -> Result<LineMap, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("the config file cannot be read again: {e}"))?;
    locate_in(&text, bench, loaded)
}

/// [`locate`] over the file's text; every refusal reads after "since".
fn locate_in(text: &str, bench: &str, loaded: &str) -> Result<LineMap, String> {
    let document = toml_edit::Document::parse(text)
        .map_err(|e| format!("the config file no longer parses: {e}"))?;
    let item = document
        .as_item()
        .get("bench")
        .and_then(|b| b.get(bench))
        .and_then(|b| b.get("script"))
        .ok_or_else(|| format!("the config file no longer sets bench.{bench}.script"))?;
    let decoded_by_toml = item
        .as_str()
        .ok_or_else(|| format!("the config file's bench.{bench}.script is not a string"))?;
    if decoded_by_toml != loaded {
        return Err("the config file has changed since it was loaded".into());
    }
    let span = item
        .span()
        .ok_or_else(|| "the TOML parser kept no position for the string".to_owned())?;
    let raw = text
        .get(span.clone())
        .ok_or_else(|| "the string's position is outside the file".to_owned())?;
    let first_line = line_of(text, span.start);
    let decoded = decode(raw, first_line)?;
    if decoded.bytes != loaded.as_bytes() {
        return Err("the string decodes differently here than in the TOML parser".into());
    }
    Ok(LineMap {
        lines: lua_lines(&decoded),
    })
}

/// 1-based line of byte `offset`.
fn line_of(text: &str, offset: usize) -> u32 {
    let newlines = text.as_bytes()[..offset]
        .iter()
        .filter(|b| **b == b'\n')
        .count();
    u32::try_from(newlines + 1).unwrap_or(u32::MAX)
}

/// A decoded string with each byte's file line, and the line decoding ended
/// on (for an empty last line).
#[derive(Debug)]
struct Decoded {
    bytes: Vec<u8>,
    lines: Vec<u32>,
    end: u32,
}

impl Decoded {
    fn push(&mut self, byte: u8, line: u32) {
        self.bytes.push(byte);
        self.lines.push(line);
    }
}

/// Decode one TOML string literal, `raw` with its delimiters, starting on
/// file line `line`.
fn decode(raw: &str, mut line: u32) -> Result<Decoded, String> {
    let (delimiter, basic) = if raw.starts_with("\"\"\"") {
        ("\"\"\"", true)
    } else if raw.starts_with("'''") {
        ("'''", false)
    } else if raw.starts_with('"') {
        ("\"", true)
    } else if raw.starts_with('\'') {
        ("'", false)
    } else {
        return Err("the value is not a string literal".into());
    };
    let multi = delimiter.len() == 3;
    if raw.len() < 2 * delimiter.len() || !raw.ends_with(delimiter) {
        return Err("the literal's delimiters do not match".into());
    }
    let mut body = &raw.as_bytes()[delimiter.len()..raw.len() - delimiter.len()];
    if multi {
        if let Some(rest) = body.strip_prefix(b"\n") {
            body = rest;
            line += 1;
        } else if let Some(rest) = body.strip_prefix(b"\r\n") {
            body = rest;
            line += 1;
        }
    }
    let mut out = Decoded {
        bytes: Vec::new(),
        lines: Vec::new(),
        end: line,
    };
    let mut i = 0;
    while i < body.len() {
        let byte = body[i];
        if byte == b'\\' && basic {
            i = escape(body, i + 1, &mut line, multi, &mut out)?;
            continue;
        }
        out.push(byte, line);
        if byte == b'\n' {
            line += 1;
        }
        i += 1;
    }
    out.end = line;
    Ok(out)
}

/// One escape whose backslash is just before `i`; returns where decoding
/// goes on.
fn escape(
    body: &[u8],
    i: usize,
    line: &mut u32,
    multi: bool,
    out: &mut Decoded,
) -> Result<usize, String> {
    let Some(&kind) = body.get(i) else {
        return Err("the literal ends inside an escape".into());
    };
    let simple = match kind {
        b'b' => Some(0x08),
        b't' => Some(b'\t'),
        b'n' => Some(b'\n'),
        b'f' => Some(0x0c),
        b'r' => Some(b'\r'),
        b'e' => Some(0x1b),
        b'"' => Some(b'"'),
        b'\\' => Some(b'\\'),
        _ => None,
    };
    if let Some(byte) = simple {
        out.push(byte, *line);
        return Ok(i + 1);
    }
    let digits = match kind {
        b'x' => 2,
        b'u' => 4,
        b'U' => 8,
        b' ' | b'\t' | b'\r' | b'\n' if multi => return line_ending_backslash(body, i, line),
        other => {
            return Err(format!(
                "the escape \\{} is not one this decoder reads",
                char::from(other)
            ));
        }
    };
    let hex = body
        .get(i + 1..i + 1 + digits)
        .and_then(|h| std::str::from_utf8(h).ok())
        .ok_or_else(|| "the string has a truncated escape".to_owned())?;
    let code = u32::from_str_radix(hex, 16).map_err(|_| format!("the escape {hex} is not hex"))?;
    let ch = char::from_u32(code).ok_or_else(|| format!("the escape {hex} is no scalar value"))?;
    let mut buf = [0; 4];
    for byte in ch.encode_utf8(&mut buf).as_bytes() {
        out.push(*byte, *line);
    }
    Ok(i + 1 + digits)
}

/// A multi-line basic string's line-ending backslash at `i - 1`: optional
/// blanks, a newline, then every blank and newline up to the next other byte
/// are dropped.
fn line_ending_backslash(body: &[u8], mut i: usize, line: &mut u32) -> Result<usize, String> {
    while matches!(body.get(i), Some(b' ' | b'\t')) {
        i += 1;
    }
    match body.get(i) {
        Some(b'\n') => {}
        Some(b'\r') if body.get(i + 1) == Some(&b'\n') => i += 1,
        _ => return Err("the string has a backslash followed by blanks but no newline".into()),
    }
    while let Some(&byte) = body.get(i) {
        match byte {
            b'\n' => *line += 1,
            b' ' | b'\t' | b'\r' => {}
            _ => break,
        }
        i += 1;
    }
    Ok(i)
}

/// The file line of each Lua line, as dellingr counts lines.
fn lua_lines(decoded: &Decoded) -> Vec<u32> {
    let bytes = &decoded.bytes;
    let mut starts = vec![0];
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte == b'\r' || byte == b'\n' {
            let paired = bytes
                .get(i + 1)
                .is_some_and(|next| (*next == b'\r' || *next == b'\n') && *next != byte);
            i += if paired { 2 } else { 1 };
            starts.push(i);
        } else {
            i += 1;
        }
    }
    starts
        .into_iter()
        .map(|start| decoded.lines.get(start).copied().unwrap_or(decoded.end))
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn map(text: &str, loaded: &str) -> Vec<u32> {
        match locate_in(text, "cat", loaded) {
            Ok(map) => map.lines,
            Err(why) => panic!("{text:?}: {why}"),
        }
    }

    #[test]
    fn a_multi_line_string_counts_from_the_line_after_its_quotes() {
        let text = "project = \"x\"\n\n[bench.cat]\nscript = \"\"\"\nfunction bench()\n  x()\nend\n\"\"\"\n";
        assert_eq!(map(text, "function bench()\n  x()\nend\n"), [5, 6, 7, 8]);
    }

    #[test]
    fn escapes_and_line_ending_backslashes_move_lines() {
        // `\n` escapes break Lua lines on one file line; a line-ending
        // backslash joins file lines 2 and 3 into Lua line 2.
        let text = "[bench.cat]\nscript = \"\"\"a\\nb \\\n   c\nd\\te\"\"\"\n";
        assert_eq!(map(text, "a\nb c\nd\te"), [2, 2, 4]);
        let single = "bench.cat.script = \"x\\ny\\r\\nz\\u00e9\"\n";
        assert_eq!(map(single, "x\ny\r\nz\u{e9}"), [1, 1, 1]);
    }

    #[test]
    fn every_toml_spelling_of_the_key_is_found() {
        let literal = "[bench]\ncat = { script = '''\nline1\nline2''' }\n";
        assert_eq!(map(literal, "line1\nline2"), [3, 4]);
        let quoted = "[bench.\"cat\"]\nscript = 'one'\n";
        assert_eq!(map(quoted, "one"), [2]);
        let crlf = "[bench.cat]\r\nscript = \"\"\"\r\na\r\nb\"\"\"\r\n";
        assert_eq!(map(crlf, "a\r\nb"), [3, 4]);
        let quotes = "[bench.cat]\nscript = \"\"\"x = '\"'\"\"\"\"\n";
        assert_eq!(
            map(quotes, "x = '\"'\""),
            [2],
            "four closing quotes keep one"
        );
    }

    #[test]
    fn a_lone_cr_breaks_a_lua_line_and_the_last_empty_line_maps_to_the_end() {
        let text = "[bench.cat]\nscript = \"\"\"\na\\rb\nc\n\"\"\"\n";
        assert_eq!(map(text, "a\rb\nc\n"), [3, 3, 4, 5]);
    }

    #[test]
    fn a_changed_or_missing_script_gives_no_map() {
        let text = "[bench.cat]\nscript = \"a\"\n";
        assert!(locate_in(text, "cat", "b").unwrap_err().contains("changed"));
        assert!(
            locate_in(text, "dog", "a")
                .unwrap_err()
                .contains("no longer sets")
        );
        assert!(locate_in("[bench.cat]\nscript = 3\n", "cat", "a").is_err());
    }
}
